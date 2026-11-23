//! Project/dependency custody and the separate observed-root check for worker admission.
use lean_toolchain::shared_writer::{LocalArtifactRead, project_artifact_roots};
use std::{
    fs::{self, File},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub struct PreparedProject {
    project: PathBuf,
    domains: Vec<PathBuf>,
    guards: LocalArtifactRead,
    ordered: Vec<(PathBuf, File)>,
}
pub(super) fn valid_module(name: &str) -> bool {
    name.split('.').all(|part| {
        part.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    })
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "prepared import roots differ or lack custody",
    )
}
impl PreparedProject {
    // The approval supplies the order. The SDK manifest graph independently supplies
    // the lock domains. Installed shim locks remain owned by Installed, and Elan
    // replacement is excluded for the lifetime of the job.
    ///
    /// # Errors
    /// Refuses unavailable custody, noncanonical roots, or a different domain set.
    pub fn capture(
        project: &Path,
        order: &[PathBuf],
        installed_shim_roots: &[PathBuf],
        release_root: &Path,
    ) -> io::Result<Self> {
        if project.canonicalize()?.as_os_str() != project.as_os_str() {
            return Err(invalid());
        }
        let domains = project_artifact_roots(project)?;
        let guards = LocalArtifactRead::try_acquire(&domains)?;
        if project_artifact_roots(project)? != domains {
            return Err(invalid());
        }
        let mut expected: Vec<_> = domains.iter().map(|d| d.join(".lake/build/lib/lean")).collect();
        expected.extend_from_slice(installed_shim_roots);
        expected.push(release_root.to_owned());
        let mut actual = order.to_vec();
        actual.sort();
        actual.dedup();
        expected.sort();
        expected.dedup();
        if actual != expected {
            return Err(invalid());
        }
        let mut ordered = Vec::with_capacity(order.len());
        for path in order {
            if path.canonicalize()?.as_os_str() != path.as_os_str() {
                return Err(invalid());
            }
            let file = File::open(path)?;
            if !file.metadata()?.is_dir() {
                return Err(invalid());
            }
            ordered.push((path.clone(), file));
        }
        Ok(Self {
            project: project.to_owned(),
            domains,
            guards,
            ordered,
        })
    }
    /// Revalidate the held roots and their observed import order.
    pub fn check_observed(&self, observed: Option<&[String]>) -> io::Result<()> {
        let roots = observed.ok_or_else(invalid)?;
        self.guards.validate()?;
        if project_artifact_roots(&self.project)? != self.domains || roots.len() != self.ordered.len() {
            return Err(invalid());
        }
        for (root, (path, held)) in roots.iter().zip(&self.ordered) {
            let named = fs::symlink_metadata(path)?;
            let opened = held.metadata()?;
            if path.as_os_str() != std::ffi::OsStr::new(root)
                || path.canonicalize()?.as_os_str() != path.as_os_str()
                || !named.is_dir()
                || (named.dev(), named.ino()) != (opened.dev(), opened.ino())
            {
                return Err(invalid());
            }
        }
        Ok(())
    }
}

/// Closed build selection, supplied by the operator/preparer, never extracted from a theorem request.
/// Each local module belongs to one SDK writer domain and uses the default Lake output layout.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportPlan {
    project: PathBuf,
    roots: Vec<PathBuf>,
    release: PathBuf,
    modules: Vec<(String, PathBuf)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    installed_release: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(try_from = "GenerationWire")]
pub struct PreparedGeneration {
    plan: ImportPlan,
    identity: [u8; 32],
    #[serde(default)]
    products: Vec<Vec<Option<String>>>,
    #[serde(skip)]
    compact: bool,
    #[serde(skip)]
    derived: bool,
}

const LAUNCH_INPUT_LIMIT: usize = 4096;

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum GenerationWire {
    Compact(CompactGeneration),
    Legacy(LegacyGeneration),
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyGeneration {
    plan: ImportPlan,
    identity: [u8; 32],
    #[serde(default)]
    products: Vec<Vec<Option<String>>>,
}

/// Paths are stored once relative to the common component prefix with the
/// project. The aggregate identity already authenticates ordered paths,
/// sources, and presence and contents of every companion file.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactGeneration {
    version: u8,
    project: PathBuf,
    roots: Vec<(usize, PathBuf)>,
    release: usize,
    modules: Vec<(String, usize)>,
    installed_release: Option<String>,
    identity: [u8; 32],
}

fn module_domain(root: &Path) -> io::Result<&Path> {
    let domain = root.ancestors().nth(4).ok_or_else(invalid)?;
    if domain.join(".lake/build/lib/lean") != root {
        return Err(invalid());
    }
    Ok(domain)
}

impl TryFrom<GenerationWire> for PreparedGeneration {
    type Error = io::Error;

    fn try_from(wire: GenerationWire) -> io::Result<Self> {
        match wire {
            GenerationWire::Legacy(record) => Ok(Self {
                plan: record.plan,
                identity: record.identity,
                products: record.products,
                compact: false,
                derived: false,
            }),
            GenerationWire::Compact(record) => {
                if !matches!(record.version, 2 | 3) || !record.project.is_absolute() {
                    return Err(invalid());
                }
                let roots = record
                    .roots
                    .into_iter()
                    .map(|(count, suffix)| {
                        let mut path = PathBuf::new();
                        let mut prefix = record.project.components();
                        for _ in 0..count {
                            path.push(prefix.next().ok_or_else(invalid)?.as_os_str());
                        }
                        if (count == 0) != suffix.is_absolute()
                            || suffix
                                .components()
                                .any(|part| matches!(part, std::path::Component::ParentDir))
                        {
                            return Err(invalid());
                        }
                        path.push(suffix);
                        Ok(path)
                    })
                    .collect::<io::Result<Vec<_>>>()?;
                let release = roots.get(record.release).ok_or_else(invalid)?.clone();
                let modules = record
                    .modules
                    .into_iter()
                    .map(|(name, index)| {
                        let root = roots.get(index).ok_or_else(invalid)?;
                        Ok((name, module_domain(root)?.to_owned()))
                    })
                    .collect::<io::Result<Vec<_>>>()?;
                Ok(Self {
                    plan: ImportPlan {
                        project: record.project,
                        roots,
                        release,
                        modules,
                        installed_release: record.installed_release,
                    },
                    identity: record.identity,
                    products: vec![],
                    compact: true,
                    derived: record.version == 3,
                })
            }
        }
    }
}

impl CompactGeneration {
    fn encode(record: &PreparedGeneration) -> io::Result<Self> {
        let plan = &record.plan;
        let root_index = |path: &Path| plan.roots.iter().position(|root| root == path).ok_or_else(invalid);
        Ok(Self {
            version: if record.derived { 3 } else { 2 },
            project: plan.project.clone(),
            roots: plan
                .roots
                .iter()
                .map(|root| {
                    let count = plan
                        .project
                        .components()
                        .zip(root.components())
                        .take_while(|(a, b)| a == b)
                        .count();
                    let suffix = root.components().skip(count).collect();
                    (count, suffix)
                })
                .collect(),
            release: root_index(&plan.release)?,
            modules: plan
                .modules
                .iter()
                .map(|(name, domain)| Ok((name.clone(), root_index(&domain.join(".lake/build/lib/lean"))?)))
                .collect::<io::Result<Vec<_>>>()?,
            installed_release: plan.installed_release.clone(),
            identity: record.identity,
        })
    }
}

fn file_hash(path: &Path) -> io::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};
    use std::io::Read as _;
    let named = fs::symlink_metadata(path)?;
    if !named.is_file() || named.nlink() != 1 || path.canonicalize()?.as_os_str() != path.as_os_str() {
        return Err(invalid());
    }
    let mut file = File::open(path)?;
    let opened = file.metadata()?;
    if (opened.dev(), opened.ino()) != (named.dev(), named.ino()) {
        return Err(invalid());
    }
    let mut hash = Sha256::new();
    let mut buf = [0; 8192];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(buf.get(..n).ok_or_else(invalid)?);
    }
    Ok(hash.finalize().into())
}

impl ImportPlan {
    /// Select only direct, non-plugin Lake modules from the approved clean build.
    pub fn new(project: PathBuf, roots: Vec<PathBuf>, release: PathBuf, modules: Vec<(String, PathBuf)>) -> Self {
        Self {
            project,
            roots,
            release,
            modules,
            installed_release: None,
        }
    }

    /// Treat one ordinary, registered Elan release as an installed dependency.
    /// The installation must remain intact while a prepared generation uses it.
    /// In-place edits to an installed release are outside this contract.
    ///
    /// # Errors
    /// Refuses a custom or unselected release and an unsupported SDK header.
    pub fn with_installed_release(mut self) -> io::Result<Self> {
        self.installed_release = Some(self.toolchain_selection()?);
        self.installed_identity()?;
        Ok(self)
    }

    fn toolchain_selection(&self) -> io::Result<String> {
        let path = self.project.join("lean-toolchain");
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() > 1024 || path.canonicalize()? != path {
            return Err(invalid());
        }
        Ok(fs::read_to_string(path)?.trim().to_owned())
    }

    fn installed_identity(&self) -> io::Result<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let refuse = || io::Error::new(io::ErrorKind::InvalidData, "unapproved installed Elan release");
        let selection = self.installed_release.as_deref().ok_or_else(refuse)?;
        let version = selection.strip_prefix("leanprover/lean4:v").ok_or_else(refuse)?;
        if version.split('.').count() != 3
            || version
                .split('.')
                .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
            || self.toolchain_selection()? != selection
        {
            return Err(refuse());
        }
        // Match the host's normal registered Elan namespace. Linked/custom
        // installations and environment-selected overrides are not accepted.
        let sysroot = std::env::home_dir()
            .ok_or_else(refuse)?
            .join(".elan/toolchains")
            .join(format!("leanprover--lean4---v{version}"));
        if sysroot.canonicalize()? != sysroot
            || self.release != sysroot.join("lib/lean")
            || self.release.canonicalize()? != self.release
        {
            return Err(refuse());
        }
        let fingerprint = lean_toolchain::ToolchainFingerprint::current();
        let header = file_hash(&sysroot.join("include/lean/lean.h"))?;
        let mut header_hex = String::with_capacity(64);
        for byte in header {
            use std::fmt::Write as _;
            write!(header_hex, "{byte:02x}").map_err(io::Error::other)?;
        }
        if header_hex != fingerprint.header_sha256 || version != fingerprint.lean_version {
            return Err(refuse());
        }
        let mut hash = Sha256::new();
        for path in [&sysroot, &self.release] {
            let metadata = fs::metadata(path)?;
            hash.update(metadata.dev().to_le_bytes());
            hash.update(metadata.ino().to_le_bytes());
        }
        hash.update(header);
        for executable in ["bin/lean", "bin/lake"] {
            hash.update(file_hash(&sysroot.join(executable))?);
        }
        let runtime = if cfg!(target_os = "macos") {
            "libleanshared.dylib"
        } else {
            "libleanshared.so"
        };
        let runtime = self.release.join(runtime);
        let metadata = fs::symlink_metadata(&runtime)?;
        if !metadata.is_file() || runtime.canonicalize()? != runtime {
            return Err(refuse());
        }
        hash.update(metadata.dev().to_le_bytes());
        hash.update(metadata.ino().to_le_bytes());
        hash.update(metadata.len().to_le_bytes());
        Ok(hash.finalize().into())
    }

    fn check_installed_order(&self) -> io::Result<()> {
        let (last, earlier) = self.roots.split_last().ok_or_else(invalid)?;
        if last != &self.release || earlier.contains(last) {
            return Err(invalid());
        }
        for entry in fs::read_dir(last)? {
            let entry = entry?;
            let path = entry.path();
            let base = if entry.file_type()?.is_dir() {
                path.file_name()
            } else if path.extension().is_some_and(|part| part == "olean") {
                path.file_stem()
            } else {
                continue;
            };
            let Some(name) = base.and_then(std::ffi::OsStr::to_str).filter(|name| valid_module(name)) else {
                continue;
            };
            // Lean assigns a whole package prefix to its first matching root.
            // Check the prefix, even if the shadow root lacks the requested leaf.
            let selected =
                lean_toolchain::olean_candidate(&self.roots, name, |p| fs::metadata(p)).map_err(io::Error::other)?;
            if selected != Some(last.join(format!("{name}.olean"))) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("installed Lean package is shadowed: {name}"),
                ));
            }
        }
        Ok(())
    }

    /// Resolve the transitive source headers in the selected project search order.
    ///
    /// # Errors
    /// Refuses missing, aliased or unapproved sources.
    pub fn derive(
        &self,
        domains: &[PathBuf],
        headers: impl FnMut(&[PathBuf]) -> io::Result<Vec<u8>>,
    ) -> io::Result<Vec<(String, PathBuf)>> {
        let closure = crate::prepared_closure::derive(
            &self.roots,
            &self.release,
            domains,
            self.modules.iter().map(|(name, _)| name.clone()),
            headers,
        )?;
        if !self.modules.iter().all(|selected| closure.contains(selected)) {
            return Err(invalid());
        }
        Ok(closure)
    }

    fn identity(&self, domains: &[PathBuf]) -> io::Result<[u8; 32]> {
        self.identity_for(domains, None)
    }

    fn identity_for(
        &self,
        domains: &[PathBuf],
        closure: Option<&[(String, PathBuf)]>,
    ) -> io::Result<[u8; 32]> {
        use sha2::{Digest, Sha256};
        if self.modules.is_empty() || self.modules.len() > 32 || self.roots.is_empty() || self.roots.len() > 64 {
            return Err(invalid());
        }
        let mut hash = Sha256::new();
        if self.installed_release.is_some() {
            self.check_installed_order()?;
            hash.update(serde_json::to_vec(&("installed-release-project-v1", self))?);
            hash.update(self.installed_identity()?);
        } else {
            hash.update(serde_json::to_vec(&("fresh-built-project-v2", self))?);
        }
        if let Some(closure) = closure {
            if self.installed_release.is_none() {
                return Err(invalid());
            }
            hash.update(serde_json::to_vec(&("derived-import-closure-v3", closure))?);
        }
        for (name, domain) in closure.unwrap_or(&self.modules) {
            let selected = lean_toolchain::olean_candidate(&self.roots, name, |p| fs::metadata(p))
                .map_err(io::Error::other)?
                .ok_or_else(invalid)?;
            if !domains.contains(domain) || !valid_module(name) {
                return Err(invalid());
            }
            let expected = domain
                .join(".lake/build/lib/lean")
                .join(name.replace('.', "/"))
                .with_extension("olean");
            if selected != expected {
                return Err(invalid());
            }
            let file = domain.join(name.replace('.', "/")).with_extension("lean");
            hash.update(file_hash(&file)?);
            hash.update(file_hash(&selected)?);
            for suffix in ["olean.server", "olean.private", "ir.sig", "ir"] {
                let part = selected.with_extension(suffix);
                match fs::symlink_metadata(&part) {
                    Ok(_) => {
                        hash.update([1]);
                        hash.update(file_hash(&part)?);
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => hash.update([0]),
                    Err(e) => return Err(e),
                }
            }
        }
        let init = lean_toolchain::olean_candidate(&self.roots, "Init", |p| fs::metadata(p))
            .map_err(io::Error::other)?
            .ok_or_else(invalid)?;
        if init != self.release.join("Init.olean") {
            return Err(invalid());
        }
        hash.update(file_hash(&init)?);
        Ok(hash.finalize().into())
    }
    // Ask the same Lean release to parse each selected source header under the
    // custody and search order used for this generation. Every local edge must
    // lead to another captured output or the selected installed release.
    fn check_edges(&self, writer: &mut lean_toolchain::shared_writer::SharedWriter) -> io::Result<()> {
        use std::{process::Command, time::Duration};
        let lean = self
            .release
            .parent()
            .and_then(Path::parent)
            .ok_or_else(invalid)?
            .join("bin/lean");
        let local: Vec<_> = self
            .modules
            .iter()
            .map(|(name, domain)| {
                domain
                    .join(".lake/build/lib/lean")
                    .join(name.replace('.', "/"))
                    .with_extension("olean")
            })
            .collect();
        let search =
            std::env::join_paths(self.roots.iter().filter(|path| *path != &self.release)).map_err(io::Error::other)?;
        for (name, domain) in &self.modules {
            let mut cmd = Command::new(&lean);
            cmd.current_dir(&self.project)
                .env("LEAN_PATH", &search)
                .arg("--deps")
                .arg(domain.join(name.replace('.', "/")).with_extension("lean"));
            let out = writer.run(&cmd, Duration::from_secs(30))?;
            if !out.status.success() {
                return Err(io::Error::other(String::from_utf8_lossy(&out.stderr).into_owned()));
            }
            for edge in std::str::from_utf8(&out.stdout).map_err(io::Error::other)?.lines() {
                let path = Path::new(edge);
                let installed = self.installed_release.is_some()
                    && path.starts_with(&self.release)
                    && path.extension().is_some_and(|suffix| suffix == "olean")
                    && path.canonicalize()? == path;
                if !path.is_absolute()
                    || (!local.iter().any(|p| p == path) && path != self.release.join("Init.olean") && !installed)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unapproved source import: {edge}"),
                    ));
                }
            }
        }
        Ok(())
    }
    /// Materialize the operator-selected dependency closure and build one local module.
    ///
    /// # Errors
    /// Refuses incomplete custody, stale outputs, failed readiness, or changed dependency bytes.
    pub fn build_cached_generation(
        self,
        writer: &mut lean_toolchain::shared_writer::SharedWriter,
        materialize: impl FnOnce(&mut lean_toolchain::shared_writer::SharedWriter) -> io::Result<()>,
    ) -> io::Result<PreparedGeneration> {
        use std::{process::Command, time::Duration};

        if self.installed_release.is_none()
            || writer.project_root() != self.project
            || !writer.covers_project(&self.project)?
        {
            return Err(invalid());
        }
        self.installed_identity()?;
        let (target, domain) = self.modules.first().ok_or_else(invalid)?;
        if domain != &self.project || !valid_module(target) {
            return Err(invalid());
        }
        let domains = project_artifact_roots(&self.project)?;
        for domain in &domains {
            if !writer.write_roots().contains(domain) {
                return Err(invalid());
            }
            match fs::symlink_metadata(domain.join(".lake/build")) {
                Err(err) if err.kind() == io::ErrorKind::NotFound => (),
                Err(err) => return Err(err),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "cached preparation requires absent output trees",
                    ));
                }
            }
        }
        let mut dependencies = self.clone();
        dependencies.modules.retain(|(_, domain)| domain != &self.project);
        if dependencies.modules.is_empty() || dependencies.modules.len() + 1 != self.modules.len() {
            return Err(invalid());
        }
        let sysroot = self.release.parent().and_then(Path::parent).ok_or_else(invalid)?;
        let selected = crate::prepared_closure::derive(
            &self.roots,
            &self.release,
            &domains,
            self.modules
                .iter()
                .filter(|(_, domain)| domain == &self.project)
                .map(|(name, _)| name.clone()),
            |files| {
                let mut command = Command::new(sysroot.join("bin/lean"));
                command
                    .current_dir(&self.project)
                    .arg("--deps-json")
                    .args(files)
                    .env("LEAN_SYSROOT", sysroot)
                    .env_remove("LEAN_PATH")
                    .env_remove("LEAN_SRC_PATH");
                let output = writer.run(&command, Duration::from_secs(30))?;
                if output.status.success() {
                    Ok(output.stdout)
                } else {
                    Err(io::Error::other(String::from_utf8_lossy(&output.stderr).into_owned()))
                }
            },
        )?;
        if selected.into_iter().collect::<std::collections::BTreeSet<_>>() != self.modules.iter().cloned().collect() {
            return Err(io::Error::other(
                "prepared roster does not match the selected source closure",
            ));
        }
        materialize(writer)?;
        self.check_edges(writer)?;
        let expected = dependencies.identity(&domains)?;
        let check_dependencies = || {
            if dependencies.identity(&domains)? != expected {
                return Err(io::Error::other("materialized dependency identity changed"));
            }
            Ok(())
        };
        let (readiness, build) = crate::module_preparation::run_module(
            target,
            |args| {
                let mut cmd = Command::new(sysroot.join("bin/lake"));
                cmd.current_dir(&self.project)
                    .args(["--no-cache"])
                    .args(args)
                    .env("LEAN_SYSROOT", sysroot)
                    .env_remove("LEAN_PATH")
                    .env_remove("LEAN_SRC_PATH")
                    .env_remove("ELAN_TOOLCHAIN");
                let output = writer.run(&cmd, Duration::from_secs(90))?;
                if !output.status.success() {
                    return Err(io::Error::other(format!(
                        "Lake {args:?} failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    )));
                }
                Ok(0)
            },
            check_dependencies,
        )?;
        if readiness != 0 || build != Some(0) {
            return Err(invalid());
        }
        check_dependencies()?;
        // The reader opens even the pinned domains with no selected objects.
        // Create only default-layout directories under the held writer.
        for domain in &domains {
            let mut directory = domain.clone();
            for component in [".lake", "build", "lib", "lean"] {
                directory.push(component);
                match fs::symlink_metadata(&directory) {
                    Ok(meta) if meta.is_dir() => (),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => fs::create_dir(&directory)?,
                    Ok(_) => return Err(invalid()),
                    Err(err) => return Err(err),
                }
            }
        }
        let identity = self.identity(&domains)?;
        self.finish_generation(identity)
    }

    /// Build the selected plugin-free default-layout project from clean output domains
    /// and capture the result before releasing the same writer.
    ///
    /// # Errors
    /// Refuses existing outputs, failed guarded preparation, or unexpected imports.
    pub fn build_generation(
        self,
        writer: &mut lean_toolchain::shared_writer::SharedWriter,
    ) -> io::Result<PreparedGeneration> {
        if writer.project_root() != self.project || !writer.covers_project(&self.project)? {
            return Err(invalid());
        }
        if self.installed_release.is_some() {
            self.installed_identity()?;
            self.check_installed_order()?;
        }
        let (target, domain) = self.modules.first().ok_or_else(invalid)?;
        if domain != &self.project || !valid_module(target) {
            return Err(invalid());
        }
        for domain in project_artifact_roots(&self.project)? {
            if !writer.write_roots().contains(&domain) {
                return Err(invalid());
            }
            match fs::symlink_metadata(domain.join(".lake/build")) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "prepared build requires absent output trees",
                    ));
                }
            }
        }
        let lake = self
            .release
            .parent()
            .and_then(Path::parent)
            .ok_or_else(invalid)?
            .join("bin/lake");
        let mut cmd = std::process::Command::new(&lake);
        cmd.current_dir(&self.project)
            .env("LAKE_ARTIFACT_CACHE", "false")
            .args(["--no-cache", "build", &format!("+{target}")]);
        if self.installed_release.is_some() {
            cmd.env(
                "LEAN_SYSROOT",
                lake.parent().and_then(Path::parent).ok_or_else(invalid)?,
            )
            .env_remove("LEAN_PATH")
            .env_remove("LEAN_SRC_PATH")
            .env_remove("ELAN_TOOLCHAIN");
        }
        let built = writer.run(&cmd, std::time::Duration::from_secs(90))?;
        if !built.status.success() {
            return Err(io::Error::other(String::from_utf8_lossy(&built.stderr).into_owned()));
        }
        let identity = self.identity(writer.write_roots())?;
        self.check_edges(writer)?;
        self.finish_generation(identity)
    }

    fn finish_generation(self, identity: [u8; 32]) -> io::Result<PreparedGeneration> {
        let products = self
            .modules
            .iter()
            .map(|(name, domain)| {
                let base = domain.join(".lake/build/lib/lean").join(name.replace('.', "/"));
                ["olean", "olean.server", "olean.private", "ir.sig", "ir"]
                    .into_iter()
                    .map(|ext| {
                        let path = base.with_extension(ext);
                        match fs::symlink_metadata(&path) {
                            Ok(_) => {
                                use std::fmt::Write as _;
                                let mut digest = String::with_capacity(64);
                                for byte in file_hash(&path)? {
                                    write!(digest, "{byte:02x}").map_err(io::Error::other)?;
                                }
                                Ok(Some(digest))
                            }
                            Err(e) if ext != "olean" && e.kind() == io::ErrorKind::NotFound => Ok(None),
                            Err(e) => Err(e),
                        }
                    })
                    .collect::<io::Result<Vec<_>>>()
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(PreparedGeneration {
            plan: self,
            identity,
            products,
            compact: false,
            derived: false,
        })
    }
}
impl PreparedGeneration {
    /// Bind a derived closure while holding the complete, exclusive producer union.
    ///
    /// # Errors
    /// Refuses unresolved sources, aliases, unexpected paths or absent artifacts.
    pub fn capture_derived(
        plan: ImportPlan,
        domains: &[PathBuf],
        headers: impl FnMut(&[PathBuf]) -> io::Result<Vec<u8>>,
    ) -> io::Result<Self> {
        let plan = plan.with_installed_release()?;
        let closure = plan.derive(domains, headers)?;
        let identity = plan.identity_for(domains, Some(&closure))?;
        Ok(Self {
            plan,
            identity,
            products: vec![],
            compact: true,
            derived: true,
        })
    }

    /// True for a generation whose exact closure is recomputed under custody.
    pub fn is_derived(&self) -> bool {
        self.derived
    }

    fn observed_identity(&self, domains: &[PathBuf]) -> io::Result<[u8; 32]> {
        if self.derived {
            let sysroot = self.plan.release.parent().and_then(Path::parent).ok_or_else(invalid)?;
            let closure = self.plan.derive(domains, |files| {
                let output = std::process::Command::new(sysroot.join("bin/lean"))
                    .current_dir(&self.plan.project)
                    .arg("--deps-json")
                    .args(files)
                    .env("LEAN_SYSROOT", sysroot)
                    .env_remove("LEAN_PATH")
                    .env_remove("LEAN_SRC_PATH")
                    .output()?;
                if !output.status.success() {
                    return Err(invalid());
                }
                Ok(output.stdout)
            })?;
            self.plan.identity_for(domains, Some(&closure))
        } else {
            self.plan.identity(domains)
        }
    }

    /// Decode a bounded operator record, including records written by older hosts.
    ///
    /// # Errors
    /// Refuses invalid encodings and oversized inputs.
    pub fn decode(input: &str) -> io::Result<Self> {
        if input.len() > LAUNCH_INPUT_LIMIT {
            return Err(invalid());
        }
        Ok(serde_json::from_str(input)?)
    }

    /// Ordered artifact search roots.
    pub fn roots(&self) -> &[PathBuf] {
        &self.plan.roots
    }

    /// Selected Lean library directory.
    pub fn release(&self) -> &Path {
        &self.plan.release
    }

    /// Selected modules and their owning project domains.
    pub fn modules(&self) -> &[(String, PathBuf)] {
        &self.plan.modules
    }

    /// Verify the legacy per-file hashes used by file verification. Version 2
    /// already authenticates the exact same file set via the aggregate identity.
    ///
    /// # Errors
    /// Refuses malformed legacy product records and changes to product bytes.
    pub fn check_products(&self) -> io::Result<()> {
        if self.compact {
            return Ok(());
        }
        if self.products.len() != self.plan.modules.len() {
            return Err(invalid());
        }
        for ((name, domain), parts) in self.plan.modules.iter().zip(&self.products) {
            let base = domain.join(".lake/build/lib/lean").join(name.replace('.', "/"));
            if !valid_module(name) || parts.len() != 5 || parts.first().and_then(Option::as_ref).is_none() {
                return Err(invalid());
            }
            for (ext, expected) in ["olean", "olean.server", "olean.private", "ir.sig", "ir"]
                .into_iter()
                .zip(parts)
            {
                let path = base.with_extension(ext);
                if let Some(expected) = expected {
                    use std::fmt::Write as _;
                    let mut actual = String::with_capacity(64);
                    for byte in file_hash(&path)? {
                        write!(actual, "{byte:02x}").map_err(io::Error::other)?;
                    }
                    if actual != *expected {
                        return Err(invalid());
                    }
                } else if !matches!(fs::symlink_metadata(&path), Err(e) if e.kind() == io::ErrorKind::NotFound) {
                    return Err(invalid());
                }
            }
        }
        Ok(())
    }

    /// Verify a captured generation against its producer identity under existing reader custody.
    ///
    /// # Errors
    /// Refuses malformed selections or changes to ordered roots, sources, or products.
    pub fn check_identity(input: &str, domains: &[PathBuf]) -> io::Result<()> {
        let generation = Self::decode(input)?;
        if generation.observed_identity(domains)? != generation.identity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared generation identity changed",
            ));
        }
        Ok(())
    }

    /// Project selected by the parent when it bound this record to its configured primary.
    pub fn project(&self) -> &Path {
        &self.plan.project
    }

    /// Bounded launch data for an explicitly selected generation. Contains no module bytes.
    ///
    /// # Errors
    /// Returns an error if the record cannot be encoded within the launch bound.
    pub fn launch_input(&self) -> io::Result<String> {
        if !self.compact {
            let legacy = serde_json::to_string(self)?;
            if legacy.len() <= LAUNCH_INPUT_LIMIT {
                return Ok(legacy);
            }
        }
        let input = serde_json::to_string(&CompactGeneration::encode(self)?)?;
        if input.len() > LAUNCH_INPUT_LIMIT {
            return Err(io::Error::other(
                "prepared generation exceeds the 4096-byte launch limit",
            ));
        }
        Ok(input)
    }
    /// Re-resolve under reader custody before any native import. Caller supplies the separately
    /// selected project, known release and installed shim roots. Keep the result through use.
    ///
    /// # Errors
    /// Refuses unselected modules, incomplete custody, or changed prepared products.
    pub fn verify_before_import(
        input: &str,
        project: &Path,
        release: &Path,
        shims: &[PathBuf],
        ordered_inputs: &[PathBuf],
        names: &[String],
    ) -> io::Result<PreparedProject> {
        let generation = Self::decode(input)?;
        generation.check_import_selection(project, release, ordered_inputs, names)?;
        let plan = &generation.plan;
        let held = PreparedProject::capture(project, &plan.roots, shims, release)?;
        if generation.observed_identity(&held.domains)? != generation.identity
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared module content changed",
            ));
        }
        Ok(held)
    }

    /// Check the immutable requested imports against a verified selection.
    ///
    /// # Errors
    /// Refuses another project, release, root order, or unapproved import.
    pub fn check_import_selection(
        &self,
        project: &Path,
        release: &Path,
        ordered_inputs: &[PathBuf],
        names: &[String],
    ) -> io::Result<()> {
        let plan = &self.plan;
        let pre_import = plan.roots.iter().filter(|path| path.as_path() != release);
        if plan.project != project || plan.release != release {
            return Err(io::Error::other("prepared project or release differs"));
        }
        if pre_import.ne(ordered_inputs.iter()) {
            return Err(io::Error::other(format!(
                "prepared order differs: expected {:?}, got {:?}",
                plan.roots, ordered_inputs
            )));
        }
        if names
            .iter()
            .any(|name| name != "Init" && !plan.modules.iter().any(|(approved, _)| approved == name))
        {
            return Err(io::Error::other(format!("unapproved prepared imports: {names:?}")));
        }
        if self.derived
            && names
                .iter()
                .filter(|name| name.as_str() != "Init")
                .ne(plan.modules.iter().map(|(name, _)| name))
        {
            return Err(io::Error::other(format!("derived prepared imports differ: {names:?}")));
        }
        Ok(())
    }
}

#[test]
#[ignore = "needs the disposable path-dependency Lake project and released standalone writer"]
fn path_dependency_custody_and_observation() -> io::Result<()> {
    use lean_toolchain::shared_writer::project_lock_path;
    use std::process::Command;
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/r2-prepared-project")
        .canonicalize()?;
    let project = base.join("project");
    let dep = project.join("deps/dep");
    let release = PathBuf::from(std::env::var_os("LEAN_SYSROOT").ok_or_else(invalid)?).canonicalize()?;
    let lake = release.join("bin/lake");
    let dep_root = dep.join(".lake/build/lib/lean");
    let main_root = project.join(".lake/build/lib/lean");
    assert!(dep_root.join("Dep.olean").is_file() && main_root.join("Root.olean").is_file());
    let builtin = release.join("lib/lean");
    let search = [dep_root.clone(), main_root.clone()];
    let order = [dep_root.clone(), main_root.clone(), builtin.clone(), builtin.clone()];
    let input = fs::read_to_string(base.join("launch-input"))?;
    let names = vec![String::from("Root")];
    let verify = || PreparedGeneration::verify_before_import(&input, &project, &builtin, &[], &search, &names);
    assert!(
        PreparedGeneration::verify_before_import(&input, &project, &builtin, &[], &search, &["Other".into()]).is_err()
    );
    assert!(
        PreparedGeneration::verify_before_import(
            &input,
            &project,
            &builtin,
            &[],
            &[main_root, dep_root.clone()],
            &names
        )
        .is_err()
    );
    let leaf = dep_root.join("Dep.olean");
    let saved = dep_root.join("original-output");
    fs::rename(&leaf, &saved)?;
    fs::write(&leaf, b"unapproved output")?;
    let refused = verify().is_err();
    fs::remove_file(&leaf)?;
    fs::rename(&saved, &leaf)?;
    assert!(refused);
    let prepared = verify()?;
    assert_eq!(
        prepared.ordered.iter().map(|(p, _)| p).collect::<Vec<_>>(),
        order.iter().collect::<Vec<_>>()
    );
    for domain in [&project, &dep] {
        let competing = File::open(project_lock_path(domain)?)?;
        assert!(matches!(competing.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
    }
    let output = Command::new(&lake)
        .args(["--no-build", "env", "lean", "--run", "Observe.lean"])
        .current_dir(&project)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(String::from_utf8_lossy(&output.stderr).into_owned()));
    }
    let observation = String::from_utf8(output.stdout).map_err(io::Error::other)?;
    let observed: Vec<_> = observation.lines().map(str::to_owned).collect();
    eprintln!("Lake/Lean observed roots: {observed:?}");
    prepared.check_observed(Some(&observed))?;
    assert!(prepared.check_observed(observed.get(..3)).is_err());
    let mut swapped = observed.clone();
    swapped.swap(0, 1);
    assert!(prepared.check_observed(Some(&swapped)).is_err());
    assert!(prepared.check_observed(None).is_err());
    assert!(prepared.check_observed(observed.get(1..)).is_err());
    // Deliberately model a noncooperating directory replacement inside this fixture.
    let moved = dep_root.with_file_name("lean-original");
    fs::rename(&dep_root, &moved)?;
    fs::create_dir(&dep_root)?;
    let refused = prepared.check_observed(Some(&observed)).is_err();
    fs::remove_dir(&dep_root)?;
    fs::rename(&moved, &dep_root)?;
    assert!(refused);
    prepared.check_observed(Some(&observed))?;
    drop(prepared);
    for domain in [&project, &dep] {
        File::open(project_lock_path(domain)?)?
            .try_lock()
            .map_err(io::Error::from)?;
    }
    Ok(())
}

#[test]
#[ignore = "one-time guarded build before the separate read admission"]
fn build_path_dependency() -> io::Result<()> {
    use lean_toolchain::shared_writer::SharedWriter;
    use std::{process::Command, time::Duration};
    let base = std::env::var_os("TEST_PREPARED_FIXTURE")
        .map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/r2-prepared-project"),
            PathBuf::from,
        )
        .canonicalize()?;
    let project = base.join("project");
    let installed_release = std::env::var_os("TEST_PREPARED_INSTALLED_RELEASE").is_some();
    let lake = PathBuf::from(std::env::var_os("LEAN_SYSROOT").ok_or_else(invalid)?).join("bin/lake");
    // First-time disposable setup: Lake writes the dependency manifests before
    // the SDK writer can resolve and lock the complete graph.
    fs::create_dir(&project)?;
    let dep = project.join("deps/dep");
    fs::create_dir_all(&dep)?;
    for root in [&project, &dep] {
        fs::write(root.join("lean-toolchain"), "leanprover/lean4:v4.36.0\n")?;
    }
    fs::write(
        project.join("lakefile.toml"),
        "name = 'root'\n[[require]]\nname = 'dep'\npath = 'deps/dep'\n[[lean_lib]]\nname = 'Root'\n",
    )?;
    fs::write(dep.join("lakefile.toml"), "name = 'dep'\n[[lean_lib]]\nname = 'Dep'\n")?;
    fs::write(dep.join("Dep.lean"), "theorem dependency_ok : True := True.intro\n")?;
    let source = "import Dep\ntheorem first_ok : True := dependency_ok\ntheorem second_ok : True := first_ok\n";
    fs::write(
        project.join("Root.lean"),
        if installed_release {
            format!("import Lean\n{source}")
        } else {
            source.to_owned()
        },
    )?;
    fs::write(
        project.join("Observe.lean"),
        "import Lean\ndef main : IO Unit := do\n  for p in (← Lean.searchPathRef.get) do IO.println p.toString\n",
    )?;
    for root in [&dep, &project] {
        let output = Command::new(&lake)
            .args(["--no-cache", "update"])
            .current_dir(root)
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(String::from_utf8_lossy(&output.stderr).into_owned()));
        }
    }
    let mut writer = SharedWriter::open(
        &base.join("tools/bin/lean-rs-writer"),
        &project,
        Duration::from_secs(10),
    )?;
    let release = lake
        .parent()
        .and_then(Path::parent)
        .ok_or_else(invalid)?
        .join("lib/lean");
    let installed = std::env::var_os("TEST_INSTALLED_SIDECAR");
    let roots = if let Some(ref path) = installed {
        let path = Path::new(path);
        let generation = path.parent().ok_or_else(invalid)?;
        let sidecar: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
        let mut roots = vec![project.join(".lake/build/lib/lean"), dep.join(".lake/build/lib/lean")];
        for root in sidecar
            .pointer("/deployment/prepared/shim_search_paths")
            .ok_or_else(invalid)?
            .as_array()
            .ok_or_else(invalid)?
        {
            roots.push(generation.join(root.as_str().ok_or_else(invalid)?).canonicalize()?);
        }
        roots.push(release.clone());
        roots
    } else if installed_release {
        vec![
            project.join(".lake/build/lib/lean"),
            dep.join(".lake/build/lib/lean"),
            release.clone(),
        ]
    } else {
        vec![
            dep.join(".lake/build/lib/lean"),
            project.join(".lake/build/lib/lean"),
            release.clone(),
            release.clone(),
        ]
    };
    let plan = ImportPlan::new(
        project.clone(),
        roots,
        release,
        vec![("Root".into(), project.clone()), ("Dep".into(), dep.clone())],
    );
    let plan = if installed_release {
        plan.with_installed_release()?
    } else {
        plan
    };
    let incomplete = ImportPlan::new(
        project.clone(),
        plan.roots.clone(),
        plan.release.clone(),
        vec![("Root".into(), project.clone())],
    );
    let incomplete = if installed_release {
        incomplete.with_installed_release()?
    } else {
        incomplete
    };
    let stale = dep.join(".lake/build/lib/lean/Dep.olean");
    fs::create_dir_all(stale.parent().ok_or_else(invalid)?)?;
    fs::write(&stale, b"untrusted stale fixture leaf")?;
    assert!(matches!(plan.clone().build_generation(&mut writer), Err(e) if e.kind()==io::ErrorKind::AlreadyExists));
    assert!(!project.join(".lake/build").exists());
    fs::remove_file(&stale)?;
    for dir in ["lib/lean", "lib", ""] {
        fs::remove_dir(dep.join(".lake/build").join(dir))?;
    }
    let generation = plan.build_generation(&mut writer)?;
    assert!(
        matches!(incomplete.check_edges(&mut writer), Err(e) if e.to_string().contains("unapproved source import"))
    );
    let output = if installed.is_some() {
        "installed-input"
    } else {
        "launch-input"
    };
    let input = generation.launch_input()?;
    fs::write(base.join(output), &input)?;
    writer.close(Duration::from_secs(10))?;
    if installed_release {
        let (_, before) = generation.plan.roots.split_last().ok_or_else(invalid)?;
        let shims = before.get(2..).ok_or_else(invalid)?;
        let verify = || {
            PreparedGeneration::verify_before_import(
                &input,
                &project,
                &generation.plan.release,
                shims,
                before,
                &["Root".into()],
            )
        };
        drop(verify()?);
        let pin = project.join("lean-toolchain");
        let original = fs::read(&pin)?;
        fs::write(&pin, "leanprover/lean4:v4.35.0\n")?;
        assert!(matches!(verify(), Err(e) if e.to_string().contains("unapproved installed Elan release")));
        fs::write(&pin, original)?;
        let shadow = before.first().ok_or_else(invalid)?.join("Lean");
        fs::create_dir(&shadow)?;
        assert!(matches!(verify(), Err(e) if e.to_string().contains("installed Lean package is shadowed: Lean")));
        fs::remove_dir(shadow)?;
        drop(verify()?);
    }
    Ok(())
}

#[test]
#[ignore = "requires the disposable installed-worker control after its parent exits"]
fn collect_installed_fixture() -> io::Result<()> {
    collect_fixture("r2-prepared-project")
}
#[test]
#[ignore = "requires the disposable installed semantic project"]
fn collect_semantic_fixture() -> io::Result<()> {
    collect_fixture("r2-semantic-project")
}
#[cfg(test)]
fn collect_fixture(directory: &str) -> io::Result<()> {
    use std::time::Duration;
    let owners = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(directory)
        .canonicalize()?
        .join("owners");
    lean_toolchain::native_guardian::collect_closed_owner_records(&owners, Duration::from_secs(10))?;
    for entry in fs::read_dir(owners)? {
        if entry?.file_type()?.is_dir() {
            return Err(io::Error::other("disposable worker generation still pending"));
        }
    }
    Ok(())
}
#[test]
#[ignore = "replaces only the disposable prepared dependency for the installed-worker control"]
fn replace_installed_fixture_dependency() -> io::Result<()> {
    use std::{process::Command, time::Duration};
    collect_installed_fixture()?;
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/r2-prepared-project")
        .canonicalize()?;
    let project = base.join("project");
    let timeout = Duration::from_secs(10);
    let mut writer =
        lean_toolchain::shared_writer::SharedWriter::open(&base.join("tools/bin/lean-rs-writer"), &project, timeout)?;
    let dep = project.join("deps/dep/.lake/build/lib/lean/Dep.olean");
    let root = project.join(".lake/build/lib/lean/Root.olean");
    assert_ne!(file_hash(&dep)?, file_hash(&root)?);
    let mut cmd = Command::new("/bin/cp");
    cmd.current_dir(&project)
        .arg(&root)
        .arg(dep.with_extension("replacement"));
    assert!(writer.run(&cmd, timeout)?.status.success());
    let mut cmd = Command::new("/bin/mv");
    cmd.current_dir(&project)
        .arg(dep.with_extension("replacement"))
        .arg(&dep);
    assert!(writer.run(&cmd, timeout)?.status.success());
    assert_eq!(file_hash(&dep)?, file_hash(&root)?);
    drop(writer);
    Ok(())
}

#[test]
#[ignore = "requires a disposable installed worker and a clean miniRat project"]
fn prepare_mini_rat() -> io::Result<()> {
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    let base = checkout.join("target/r2-semantic-project");
    let project = base.join("project");
    let sidecar: serde_json::Value = serde_json::from_slice(&fs::read(
        std::env::var_os("TEST_INSTALLED_SIDECAR").ok_or_else(invalid)?,
    )?)?;
    let deployment = sidecar.get("deployment").ok_or_else(invalid)?;
    let release = Path::new(
        deployment
            .get("sysroot")
            .ok_or_else(invalid)?
            .as_str()
            .ok_or_else(invalid)?,
    )
    .join("lib/lean");
    let mut roots = vec![project.join(".lake/build/lib/lean")];
    for shim in deployment
        .pointer("/prepared/shim_search_paths")
        .ok_or_else(invalid)?
        .as_array()
        .ok_or_else(invalid)?
    {
        roots.push(PathBuf::from(shim.as_str().ok_or_else(invalid)?));
    }
    roots.push(release.clone());
    let modules = ["LeanRsFixture.ProofAgent", "LeanRsFixture.ProofSearchFacts"]
        .map(|name| (name.to_owned(), project.clone()))
        .to_vec();
    let mut writer = lean_toolchain::shared_writer::SharedWriter::open(
        &base.join("tools/bin/lean-rs-writer"),
        &project,
        std::time::Duration::from_secs(15),
    )?;
    let record = ImportPlan::new(project, roots, release, modules)
        .build_generation(&mut writer)?
        .launch_input()?;
    fs::write(base.join("installed-input"), record)
}

#[test]
fn versioned_launch_encoding_keeps_legacy_and_long_paths() -> io::Result<()> {
    let legacy = r#"{"plan":{"project":"/fixture","roots":["/fixture/.lake/build/lib/lean","/elan/lib/lean"],"release":"/elan/lib/lean","modules":[["Root","/fixture"]]},"identity":[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],"products":[]}"#;
    let record = PreparedGeneration::decode(legacy)?;
    assert_eq!(record.launch_input()?, legacy);

    let workspace = PathBuf::from("/workspace")
        .join("long-segment".repeat(16))
        .join("project");
    let release = PathBuf::from("/elan/lib/lean");
    let domains: Vec<_> = (0..12)
        .map(|i| workspace.join("packages").join(format!("dep{i}")))
        .collect();
    let modules = domains
        .iter()
        .enumerate()
        .map(|(i, domain)| (format!("Mod{i}"), domain.clone()))
        .collect();
    let roots = domains
        .iter()
        .map(|domain| domain.join(".lake/build/lib/lean"))
        .chain([release.clone()])
        .collect();
    let record = PreparedGeneration {
        plan: ImportPlan::new(workspace, roots, release, modules),
        identity: [42; 32],
        products: vec![vec![Some("ab".repeat(32)); 5]; 12],
        compact: false,
        derived: false,
    };
    let compact = record.launch_input()?;
    let decoded = PreparedGeneration::decode(&compact)?;
    assert!(decoded.compact);
    assert_eq!(decoded.roots(), record.roots());
    assert_eq!(decoded.modules(), record.modules());
    assert_eq!(decoded.identity, record.identity);
    assert_eq!(decoded.launch_input()?, compact);
    Ok(())
}
