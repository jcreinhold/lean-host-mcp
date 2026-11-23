//! The installed worker's first supported operation is inspection of builtin Init
//! in the installer's empty smoke project. Other project operations require a
//! separate, project-specific admission.
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
use lean_rs_worker_protocol::types::LeanWorkerPreparedHostArtifacts;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
struct Sidecar {
    deployment: Deployment,
}
#[derive(Deserialize)]
struct Deployment {
    worker_sha256: String,
    sysroot: PathBuf,
    prepared: LeanWorkerPreparedHostArtifacts,
    files: BTreeMap<PathBuf, String>,
}
impl Deployment {
    fn resolve_generation(&mut self, dir: &Path) -> io::Result<()> {
        let resolve = |path: &Path| {
            if !path.starts_with("shims") || path.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
                return Err(invalid("invalid installed generation-relative path"));
            }
            Ok(dir.join(path))
        };
        let expand = |path: &str| -> io::Result<String> {
            resolve(Path::new(path))?
                .into_os_string()
                .into_string()
                .map_err(|_| invalid("invalid installed path encoding"))
        };
        let p = &mut self.prepared;
        p.shim_manifest_path = expand(&p.shim_manifest_path)?;
        for path in p.artifact_roots.iter_mut().chain(&mut p.shim_search_paths) {
            *path = expand(path)?;
        }
        self.files = std::mem::take(&mut self.files)
            .into_iter()
            .map(|(path, hash)| {
                if path.starts_with(&self.sysroot) {
                    Ok((path, hash))
                } else {
                    Ok((resolve(&path)?, hash))
                }
            })
            .collect::<io::Result<_>>()?;
        Ok(())
    }
}
pub(super) struct Installed {
    record: Deployment,
    launch_input: Option<String>,
    semantic: Option<crate::semantic_selection::Semantic>,
    held_project: std::sync::OnceLock<lean_host_mcp_worker::prepared_project::PreparedProject>,
    project: PathBuf,
    guards: Vec<fs::File>,
}
pub(super) fn deny() -> lean_rs::LeanError {
    lean_rs::error::host_internal("request outside installed Init deployment")
}
pub(super) fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
pub(super) fn digest(path: &Path) -> io::Result<String> {
    if path.canonicalize()? != path || !path.is_file() {
        return Err(invalid("expected canonical installed file"));
    }
    let mut hash = Sha256::new();
    let mut file = fs::File::open(path)?;
    let mut buffer = [0_u8; 8192];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(buffer.get(..n).ok_or_else(|| invalid("invalid input byte count"))?);
    }
    let mut text = String::with_capacity(64);
    for b in hash.finalize() {
        use std::fmt::Write as _;
        let _ = write!(text, "{b:02x}");
    }
    Ok(text)
}
fn expected_roots(r: &Deployment) -> impl Iterator<Item = PathBuf> + '_ {
    r.prepared.shim_search_paths.iter().map(PathBuf::from)
}
impl Installed {
    pub(super) fn prepared(&self) -> &LeanWorkerPreparedHostArtifacts {
        &self.record.prepared
    }

    pub(super) fn open() -> io::Result<Self> {
        let worker = std::env::current_exe()?.canonicalize()?;
        let dir = worker.parent().ok_or_else(|| invalid("missing install directory"))?;
        let mut bytes = Vec::new();
        fs::File::open(dir.join("worker.json"))?
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1024 * 1024 {
            return Err(invalid("oversized installed sidecar"));
        }
        let mut record = serde_json::from_slice::<Sidecar>(&bytes)?.deployment;
        record.resolve_generation(dir)?;
        if digest(&worker)? != record.worker_sha256 {
            return Err(invalid("installed worker digest mismatch"));
        }
        let project = dir.join("smoke-project");
        let mut roots: Vec<_> = record.prepared.artifact_roots.iter().map(PathBuf::from).collect();
        if roots
            != [
                dir.join("shims/lean-rs-host-shims"),
                dir.join("shims/lean-rs-interop-shims"),
            ]
        {
            return Err(invalid("invalid installed shim domains"));
        }
        roots.push(project.clone());
        roots.sort();
        let guards = roots
            .iter()
            .map(|r| {
                let lock = lean_toolchain::shared_writer::project_lock_path(r)?;
                let file = fs::File::open(&lock)?;
                file.try_lock_shared().map_err(io::Error::from)?;
                use std::os::unix::fs::MetadataExt as _;
                let held = file.metadata()?;
                let named = fs::symlink_metadata(&lock)?;
                if !named.is_file() || held.nlink() != 1 || (held.dev(), held.ino()) != (named.dev(), named.ino()) {
                    return Err(invalid("installed reader lock identity changed"));
                }
                Ok(file)
            })
            .collect::<io::Result<Vec<_>>>()?;
        if record.sysroot
            != Path::new(
                option_env!("LEAN_HOST_COMPILED_SYSROOT")
                    .ok_or_else(|| invalid("worker build lacks stable Lean prefix"))?,
            )
            || std::env::vars_os().any(|(k, _)| {
                let k = k.to_string_lossy();
                k.starts_with("DYLD_")
                    || k.starts_with("__XPC_DYLD_")
                    || matches!(&*k, "LD_PRELOAD" | "LD_LIBRARY_PATH" | "LD_AUDIT")
            })
            || std::env::var_os("LEAN_PATH").is_some()
            || std::env::var_os("LEAN_SYSROOT").map(PathBuf::from).as_ref() != Some(&record.sysroot)
        {
            return Err(invalid("unapproved Lean runtime environment"));
        }
        for (path, expected) in &record.files {
            if digest(path)? != *expected {
                return Err(invalid("installed file digest mismatch"));
            }
        }
        let p = &record.prepared;
        let manifest = Path::new(&p.shim_manifest_path);
        let m = lean_toolchain::CapabilityManifest::from_bytes(manifest, p.shim_manifest_json.as_bytes())
            .map_err(|e| invalid(e.message()))?;
        let [dep] = m.dependencies.as_slice() else {
            return Err(invalid("wrong dependency count"));
        };
        if m.package != "lean_rs_host_shims"
            || m.module != "LeanRsHostShims"
            || !dep.module_initializer().is_some_and(|init| {
                init.module_name() == "LeanRsInterop" && init.package_name() == "lean_rs_interop_shims"
            })
        {
            return Err(invalid("unapproved installed initializer"));
        }
        let primary = m.primary_dylib.as_path();
        let interop = dep.path_ref();
        for asset in [primary, interop, manifest] {
            if !asset.starts_with(dir.join("shims")) || !record.files.contains_key(asset) {
                return Err(invalid("unbound installed image or manifest"));
            }
        }
        let mut expected = Vec::new();
        for (image, package) in [(interop, "lean-rs-interop-shims"), (primary, "lean-rs-host-shims")] {
            if !image.starts_with(dir.join("shims").join(package)) {
                return Err(invalid("wrong image domain"));
            }
            expected.push(
                image
                    .parent()
                    .ok_or_else(|| invalid("missing image directory"))?
                    .join("lean")
                    .canonicalize()?,
            );
        }
        if p.shim_search_paths.iter().map(PathBuf::from).ne(expected)
            || fs::read(manifest)? != p.shim_manifest_json.as_bytes()
        {
            return Err(invalid("installed manifest or ordered shim roots differ"));
        }
        for rel in ["include/lean/lean.h", "lib/lean/Init.olean"] {
            if !record.files.contains_key(&record.sysroot.join(rel)) {
                return Err(invalid("unbound installed Lean release"));
            }
        }
        for root in std::iter::once(project.join(".lake/build/lib/lean")).chain(expected_roots(&record)) {
            if root.canonicalize()? != root {
                return Err(invalid("aliased installed import directory"));
            }
            for name in ["Init", "Init.olean"] {
                match fs::symlink_metadata(root.join(name)) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Ok(_) => return Err(invalid("earlier import root shadows builtin Init")),
                    Err(e) => return Err(e),
                }
            }
        }
        let launch_input = std::env::var_os("LEAN_RS_WORKER_APPLICATION_INPUT")
            .map(|s| {
                s.into_string()
                    .map_err(|_| invalid("invalid prepared generation encoding"))
            })
            .transpose()?;
        if launch_input.as_ref().is_some_and(|s| s.len() > 4096) {
            return Err(invalid("oversized prepared generation"));
        }
        let mut semantic = None;
        let mut launch_input = launch_input;
        if let Some(input) = launch_input.as_deref() {
            let value: serde_json::Value = serde_json::from_str(input)?;
            if value.get("semantic_v1").is_some() {
                let selected: crate::semantic_selection::Selection = serde_json::from_value(value)?;
                let (prepared, admitted) =
                    crate::semantic_selection::Semantic::open(selected.semantic_v1, dir, &record.sysroot, &bytes)?;
                launch_input = Some(prepared);
                semantic = Some(admitted);
            }
        }
        Ok(Self {
            semantic,
            launch_input,
            held_project: std::sync::OnceLock::new(),
            record,
            project,
            guards,
        })
    }
}
macro_rules! deny_source {
    ($name:ident,$ty:ty) => {
        fn $name<'c>(
            &self,
            _: WorkerSourceContext<'_, 'c>,
            _: $ty,
            _: HostSourceOptions,
        ) -> LeanResult<TrustedModuleSource<'static, 'c, $ty>> {
            Err(deny())
        }
    };
}
impl WorkerAuthority for Installed {
    #[allow(
        unsafe_code,
        reason = "the installer binds the exact shipped shim products, runtime and held artifact domains"
    )]
    fn host(
        &self,
        runtime: &'static LeanRuntime,
        p: &LeanPreparedHostArtifacts,
    ) -> LeanResult<LoadedHostArtifacts<'static>> {
        let d = &self.record.prepared;
        if p.manifest_path() != Path::new(&d.shim_manifest_path)
            || p.manifest_json() != d.shim_manifest_json
            || p.artifact_roots().iter().ne(d.artifact_roots.iter().map(Path::new))
            || p.shim_search_paths()
                .iter()
                .ne(d.shim_search_paths.iter().map(Path::new))
        {
            return Err(deny());
        }
        let guards = self
            .guards
            .iter()
            .map(fs::File::try_clone)
            .collect::<io::Result<Vec<_>>>()
            .map_err(|_| deny())?;
        // SAFETY: only the verified installer-produced SDK manifest and images pass the comparisons.
        // Guards retain the cooperating workspaces while the official runtime remains installed.
        unsafe {
            LoadedHostArtifacts::load(
                runtime,
                p.manifest_path().into(),
                p.manifest_json().as_bytes().to_vec(),
                p.artifact_roots().to_vec(),
                p.shim_search_paths().to_vec(),
                guards,
            )
        }
    }
    #[allow(
        unsafe_code,
        reason = "the installed application retains the approved import generation and checks the exact import profile"
    )]
    fn imports<'c>(
        &self,
        cap: &'c TrustedHostCapability<'static>,
        paths: Vec<String>,
        names: Vec<String>,
        options: HostImportOptions,
    ) -> LeanResult<TrustedHostImports<'static, 'c>> {
        if let Some(input) = &self.launch_input {
            use lean_host_mcp_worker::prepared_project::PreparedGeneration;
            let generation: PreparedGeneration = serde_json::from_str(input).map_err(|_| deny())?;
            let ordered = paths.iter().map(PathBuf::from).collect::<Vec<_>>();
            if options != HostImportOptions::default() {
                return Err(deny());
            }
            let held = PreparedGeneration::verify_before_import(
                input,
                generation.project(),
                &self.record.sysroot.join("lib/lean"),
                &expected_roots(&self.record).collect::<Vec<_>>(),
                &ordered,
                &names,
            )
            .map_err(|e| lean_rs::error::host_internal(format!("prepared generation refused before import: {e}")))?;
            self.held_project.get_or_init(|| held);
            // SAFETY: the operator selects a trusted fresh-build record at server startup.
            // The matched launcher replaces inherited input. The verified digest covers the
            // approved plugin-free modules and their resolved dependencies, and this authority
            // retains project and shim read custody. Registered-release records also
            // bind the selected installed Elan release and refuse prior-root shadowing.
            // The installed release must stay intact. Only default import options reach here.
            return Ok(unsafe { cap.import_scope_with_options(paths, names, options) });
        }
        let expected = std::iter::once(self.project.join(".lake/build/lib/lean"))
            .chain(self.record.prepared.shim_search_paths.iter().map(PathBuf::from))
            .collect::<Vec<_>>();
        if paths.iter().map(PathBuf::from).ne(expected.iter().cloned())
            || !(names.is_empty() || names == ["Init"])
            || options != HostImportOptions::default()
        {
            return Err(deny());
        }
        for root in expected {
            for name in ["Init", "Init.olean"] {
                match fs::symlink_metadata(root.join(name)) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    _ => return Err(deny()),
                }
            }
        }
        // SAFETY: the retained application guards cover all earlier roots in exact order; Init
        // resolves to the trusted official runtime, with no project or shim Init namespace.
        Ok(unsafe { cap.import_scope_with_options(paths, names, options) })
    }
    fn validate_source_context(&self, _: &WorkerSourceContext<'_, '_>, observed: Option<&[String]>) -> LeanResult<()> {
        if self.launch_input.is_some() {
            self.held_project
                .get()
                .ok_or_else(deny)?
                .check_observed(observed)
                .map_err(|e| lean_rs::error::host_internal(e.to_string()))?;
        }
        Ok(())
    }
    fn project(
        &self,
        _: &'static LeanRuntime,
        _: &Path,
        _: &LeanModuleInitializer,
    ) -> LeanResult<(LeanLibrary<'static>, LeanModuleInitializer)> {
        Err(deny())
    }
    fn manifest_read(&self, _: &Path) -> LeanResult<WorkerManifestRead> {
        Err(deny())
    }
    fn worker(
        &self,
        runtime: &'static LeanRuntime,
        path: &Path,
        bytes: &[u8],
    ) -> LeanResult<AuthorizedWorkerSnapshot<'static>> {
        self.semantic.as_ref().ok_or_else(deny)?.snapshot(runtime, path, bytes)
    }
    fn authorize_worker_string_call(&self, call: lean_rs_worker_child::WorkerStringCall<'_>) -> LeanResult<()> {
        self.semantic.as_ref().ok_or_else(deny)?.command(&call)
    }
    fn term<'c>(
        &self,
        _: WorkerSourceContext<'_, 'c>,
        _: String,
        _: HostSourceOptions,
    ) -> LeanResult<TrustedHostTerm<'static, 'c>> {
        Err(deny())
    }
    fn kernel<'c>(
        &self,
        _: WorkerSourceContext<'_, 'c>,
        _: String,
        _: HostSourceOptions,
    ) -> LeanResult<TrustedHostKernelRequest<'static, 'c>> {
        Err(deny())
    }
    deny_source!(query, HostModuleQuery);
    deny_source!(batch, HostModuleBatch);
    deny_source!(cached, HostCachedModuleBatch);
    deny_source!(proof, ProofAttemptRequest);
    deny_source!(verify, DeclarationVerificationRequest);
    deny_source!(verify_batch, DeclarationVerificationBatchRequest);
}
