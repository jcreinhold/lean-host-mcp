//! The installed worker serves one Lake project from that project's own build.
//!
//! The host names the project's import roots at launch, in Lean's search
//! order, through `LEAN_RS_WORKER_APPLICATION_INPUT`. Without that input the
//! worker serves the installer's empty smoke project. For its whole life the
//! worker holds shared the lock of every artifact domain of the project and of
//! the installed shims. A worker that cannot take custody of the project at
//! launch holds only the shims' locks and refuses each import with the reason,
//! rather than exit before the SDK child connects. It admits an import or a
//! source request only when Lean searches exactly the launch roots, then the
//! shim roots, then the release, and only while no launch root holds a module
//! package whose compiled code this process runs: the release's or the shims'.
//! It refuses project libraries, plugins, terms, kernel requests and worker
//! commands.
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

/// Where Lake writes a package's `.olean` files, relative to the package.
const BUILD_LIB: &str = ".lake/build/lib/lean";
/// Most launch roots: the SDK takes custody of at most 32 packages of one Lake
/// workspace, and each package gives one root.
const MAX_LAUNCH_ROOTS: usize = 32;

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
    /// The roots every import names, in order: the launch roots, then the shim roots.
    paths: Vec<String>,
    /// How many of `paths` the host named at launch.
    launch: usize,
    /// The release's module directory, which Lean searches after `paths`.
    release_root: String,
    /// Module packages of the release, whose compiled code this process runs.
    release: Vec<String>,
    /// Module packages of the release and of the shims.
    native: Vec<String>,
    guards: Vec<fs::File>,
    /// Why this worker could not take custody of its project at launch. While
    /// set, it holds no project lock and refuses every import.
    refusal: Option<io::Error>,
}
pub(super) fn deny() -> lean_rs::LeanError {
    lean_rs::error::host_internal("request outside this worker's project deployment")
}
fn refuse(error: &io::Error) -> lean_rs::LeanError {
    lean_rs::error::host_internal(format!("request refused: {error}"))
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
/// The import roots the host named at launch, in Lean's search order, or the
/// smoke project's one root when it named none.
fn launch_roots(dir: &Path) -> io::Result<Vec<String>> {
    let roots = match std::env::var_os("LEAN_RS_WORKER_APPLICATION_INPUT") {
        Some(input) => serde_json::from_slice::<Vec<String>>(input.as_encoded_bytes())?,
        None => vec![
            dir.join("smoke-project")
                .join(BUILD_LIB)
                .into_os_string()
                .into_string()
                .map_err(|_| invalid("invalid installed path encoding"))?,
        ],
    };
    if roots.is_empty()
        || roots.len() > MAX_LAUNCH_ROOTS
        || roots
            .iter()
            .any(|root| !Path::new(root).is_absolute() || !Path::new(root).ends_with(BUILD_LIB))
    {
        return Err(invalid("launch roots are not Lake build outputs"));
    }
    Ok(roots)
}
/// The package directory whose Lake build output is `root`.
fn package(root: &Path) -> io::Result<&Path> {
    root.ancestors()
        .nth(4)
        .ok_or_else(|| invalid("launch root has no package directory"))
}
/// `path` with every symbolic link in its longest existing prefix resolved.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    let mut missing = Vec::new();
    let mut ancestor = path;
    loop {
        match ancestor.canonicalize() {
            Ok(resolved) => return Ok(missing.iter().rev().fold(resolved, |path, name| path.join(name))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| invalid("import root has no existing ancestor"))?,
                );
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| invalid("import root has no existing ancestor"))?;
            }
            Err(error) => return Err(error),
        }
    }
}
/// Whether `root`, with its symbolic links resolved, lies in the `.lake`
/// directory of a held domain, as the host's own readers require.
fn covered(root: &Path, domains: &[PathBuf]) -> io::Result<bool> {
    Ok(resolve(root)?
        .ancestors()
        .find(|ancestor| ancestor.file_name().is_some_and(|name| name == ".lake"))
        .and_then(Path::parent)
        .is_some_and(|package| domains.iter().any(|domain| domain == package)))
}
/// Take the lock of each artifact domain shared and keep it. The host holds
/// the same locks shared for the request that starts this worker, so no
/// cooperating writer holds one now, and none can take one while this lives.
fn hold(domains: &[PathBuf]) -> io::Result<Vec<fs::File>> {
    use std::os::unix::fs::MetadataExt as _;
    let mut locks = domains
        .iter()
        .map(|root| lean_toolchain::shared_writer::project_lock_path(root))
        .collect::<io::Result<Vec<_>>>()?;
    locks.sort();
    locks.dedup();
    locks
        .iter()
        .map(|lock| {
            let file = fs::File::open(lock)?;
            file.try_lock_shared().map_err(io::Error::from)?;
            let held = file.metadata()?;
            let named = fs::symlink_metadata(lock)?;
            if !named.is_file() || held.nlink() != 1 || (held.dev(), held.ino()) != (named.dev(), named.ino()) {
                return Err(invalid("artifact lock identity changed"));
            }
            if !lean_toolchain::shared_writer::previous_writer_stopped(&file)? {
                return Err(invalid("an earlier artifact writer may still run"));
            }
            Ok(file)
        })
        .collect()
}
/// Take custody of the project whose build the launch roots `launch` name:
/// hold shared the lock of each of its artifact domains, and check that each
/// launch root lies in one of them.
fn custody(launch: &[String]) -> io::Result<Vec<fs::File>> {
    let project = package(Path::new(launch.first().ok_or_else(|| invalid("no launch root"))?))?;
    if project.canonicalize()? != project {
        return Err(invalid("launch project is not canonical"));
    }
    let domains = lean_toolchain::shared_writer::project_artifact_roots(project)?;
    let guards = hold(&domains)?;
    if lean_toolchain::shared_writer::project_artifact_roots(project)? != domains {
        return Err(invalid(
            "project artifact domains changed while the worker took custody",
        ));
    }
    for root in launch {
        if !covered(Path::new(root), &domains)? {
            return Err(invalid("launch root outside the project's artifact domains"));
        }
    }
    Ok(guards)
}
/// Module packages a root offers Lean, which gives a whole package to the
/// first root that holds a directory or an `.olean` file of that name.
fn module_packages(root: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let name = if entry.file_type()?.is_dir() {
            path.file_name()
        } else if path.extension().is_some_and(|ext| ext == "olean") {
            path.file_stem()
        } else {
            continue;
        };
        if let Some(name) = name
            .and_then(std::ffi::OsStr::to_str)
            .filter(|name| !name.contains('.'))
        {
            names.push(name.to_owned());
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}
impl Installed {
    /// The prepared shim artifacts, for the host's readiness check. Fails
    /// when this worker would refuse every import.
    pub(super) fn ready(&self) -> io::Result<&LeanWorkerPreparedHostArtifacts> {
        if let Some(error) = &self.refusal {
            return Err(io::Error::new(error.kind(), error.to_string()));
        }
        self.check_roots()?;
        Ok(&self.record.prepared)
    }

    /// Check the installation and take custody of the project. When custody
    /// fails, the worker still starts, holds only the shims' locks and refuses
    /// every import with the reason, since a worker that exits before the SDK
    /// child connects makes the server wait out its startup deadline.
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
        let shim_domains: Vec<_> = record.prepared.artifact_roots.iter().map(PathBuf::from).collect();
        if shim_domains
            != [
                dir.join("shims/lean-rs-host-shims"),
                dir.join("shims/lean-rs-interop-shims"),
            ]
        {
            return Err(invalid("invalid installed shim domains"));
        }
        let mut guards = hold(&shim_domains)?;
        let (launch, refusal) = match launch_roots(dir).and_then(|roots| Ok((custody(&roots)?, roots))) {
            Ok((project, roots)) => {
                guards.extend(project);
                (roots, None)
            }
            Err(error) => (Vec::new(), Some(error)),
        };
        let compiled = option_env!("LEAN_HOST_COMPILED_SYSROOT")
            .ok_or_else(|| invalid("worker build lacks stable Lean prefix"))?;
        if record.sysroot != Path::new(compiled) {
            return Err(invalid(
                "the installed Lean prefix differs from the one this worker was built with",
            ));
        }
        if let Some(name) = std::env::vars_os()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .find(|k| {
                k.starts_with("DYLD_")
                    || k.starts_with("__XPC_DYLD_")
                    || matches!(k.as_str(), "LD_PRELOAD" | "LD_LIBRARY_PATH" | "LD_AUDIT")
            })
        {
            return Err(invalid(&format!("unapproved Lean runtime environment: {name} is set")));
        }
        if std::env::var_os("LEAN_PATH").is_some() {
            return Err(invalid("unapproved Lean runtime environment: LEAN_PATH is set"));
        }
        let sysroot = std::env::var("LEAN_SYSROOT").unwrap_or_default();
        if Path::new(&sysroot) != record.sysroot {
            return Err(invalid(
                "unapproved Lean runtime environment: LEAN_SYSROOT is not the installed Lean prefix",
            ));
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
        let release = module_packages(&record.sysroot.join("lib/lean"))?;
        let mut native = release.clone();
        for root in &p.shim_search_paths {
            native.extend(module_packages(Path::new(root))?);
        }
        native.sort();
        native.dedup();
        let count = launch.len();
        let mut paths = launch;
        paths.extend(p.shim_search_paths.iter().cloned());
        Ok(Self {
            record,
            paths,
            launch: count,
            // Lean appends `lib/lean` to `LEAN_SYSROOT` with its own separator.
            release_root: format!("{sysroot}/lib/lean"),
            release,
            native,
            guards,
            refusal,
        })
    }

    /// Refuse a request while this worker holds no project, or while a root
    /// shadows a compiled package.
    fn admit(&self) -> LeanResult<()> {
        if let Some(error) = &self.refusal {
            return Err(refuse(error));
        }
        self.check_roots().map_err(|e| refuse(&e))
    }

    /// Refuse a launch root that holds a release or shim package, or a shim
    /// root that holds a release package. Lean would read that root's `.olean`
    /// in place of the one that matches the code compiled into this process.
    fn check_roots(&self) -> io::Result<()> {
        for (index, root) in self.paths.iter().enumerate() {
            let names = if index < self.launch {
                &self.native
            } else {
                &self.release
            };
            for name in names {
                for entry in [
                    Path::new(root).join(name),
                    Path::new(root).join(format!("{name}.olean")),
                ] {
                    match fs::symlink_metadata(&entry) {
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Ok(_) => return Err(invalid(&format!("import root {root} shadows compiled package {name}"))),
                        Err(e) => return Err(e),
                    }
                }
            }
        }
        Ok(())
    }

    /// Every root Lean searches for this worker's imports, in its order.
    fn resolved(&self) -> Vec<String> {
        self.paths
            .iter()
            .cloned()
            .chain(std::iter::once(self.release_root.clone()))
            .collect()
    }
}
/// Grant one source operation of an admitted session.
macro_rules! grant_source {
    ($name:ident, $ty:ty, $scope:ident) => {
        #[allow(
            unsafe_code,
            reason = "validate_source_context admitted this session's exact roots before each grant"
        )]
        fn $name<'c>(
            &self,
            context: WorkerSourceContext<'_, 'c>,
            request: $ty,
            options: HostSourceOptions,
        ) -> LeanResult<TrustedModuleSource<'static, 'c, $ty>> {
            // SAFETY: the child runs `validate_source_context` on this context
            // before each grant. It checked that these imports came from this
            // worker's `imports` scope with default options and no plugin, that
            // Lean searched exactly the launch roots, the shim roots and the
            // release root, and that no launch root holds a release or shim
            // package. `resolved` passes those roots in that order. No
            // cooperating writer can change an artifact under them while the
            // worker holds their domains, and the process-wide caches hold only
            // environments this worker imported through them. The caller's
            // source, header and options run in the release's own elaborator
            // and kernel, with no plugin or project library loaded, as Lean
            // would run the same file for that caller.
            unsafe {
                context.capability.$scope(
                    context.environment,
                    context.imports,
                    self.resolved(),
                    request,
                    options,
                )
            }
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
        reason = "the worker checks the exact roots it was launched for and holds every domain under them"
    )]
    fn imports<'c>(
        &self,
        cap: &'c TrustedHostCapability<'static>,
        paths: Vec<String>,
        names: Vec<String>,
        options: HostImportOptions,
    ) -> LeanResult<TrustedHostImports<'static, 'c>> {
        self.admit()?;
        if paths != self.paths {
            return Err(lean_rs::error::host_internal(
                "import roots differ from this worker's launch roots",
            ));
        }
        if options != HostImportOptions::default() {
            return Err(deny());
        }
        // SAFETY: `paths` are this worker's launch roots in the host's order,
        // then the verified shim roots; Lean searches the installed release
        // after them. The launch roots hold the build of the project the host
        // serves, which `lean` itself would load for the same imports. The
        // worker holds every artifact domain under these roots shared for its
        // whole life, so no cooperating writer can replace an artifact while
        // this scope lives. No launch root holds a release or shim package, so
        // each module whose compiled code runs here loads its own release or
        // shim `.olean`. No plugin or project library loads, so project modules
        // have no compiled code here and only Lean's interpreter can run
        // theirs; Lean refuses an `.olean` that another release wrote. Only
        // default import options reach here.
        Ok(unsafe { cap.import_scope_with_options(paths, names, options) })
    }
    fn validate_source_context(
        &self,
        context: &WorkerSourceContext<'_, '_>,
        observed: Option<&[String]>,
    ) -> LeanResult<()> {
        let imports = context.imports;
        imports.check_capability(context.capability)?;
        if observed != Some(self.resolved().as_slice())
            || !imports.matches_request(&self.paths, imports.imports())
            || imports.plugin().is_some()
            || imports.options() != &HostImportOptions::default()
        {
            return Err(deny());
        }
        self.admit()
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
    fn worker(&self, _: &'static LeanRuntime, _: &Path, _: &[u8]) -> LeanResult<AuthorizedWorkerSnapshot<'static>> {
        Err(deny())
    }
    fn authorize_worker_string_call(&self, _: lean_rs_worker_child::WorkerStringCall<'_>) -> LeanResult<()> {
        Err(deny())
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
    grant_source!(query, HostModuleQuery, module_query_scope);
    grant_source!(batch, HostModuleBatch, module_batch_scope);
    grant_source!(cached, HostCachedModuleBatch, cached_module_batch_scope);
    grant_source!(proof, ProofAttemptRequest, proof_attempt_scope);
    grant_source!(verify, DeclarationVerificationRequest, verification_scope);
    grant_source!(
        verify_batch,
        DeclarationVerificationBatchRequest,
        verification_batch_scope
    );
}
