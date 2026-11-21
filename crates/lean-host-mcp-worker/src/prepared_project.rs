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
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedGeneration {
    plan: ImportPlan,
    identity: [u8; 32],
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
        }
    }
    fn identity(&self, domains: &[PathBuf]) -> io::Result<[u8; 32]> {
        use sha2::{Digest, Sha256};
        if self.modules.is_empty() || self.modules.len() > 32 || self.roots.is_empty() || self.roots.len() > 64 {
            return Err(invalid());
        }
        let mut hash = Sha256::new();
        hash.update(serde_json::to_vec(&("fresh-built-project-v2", self))?);
        for (name, domain) in &self.modules {
            let selected = lean_toolchain::olean_candidate(&self.roots, name, |p| fs::metadata(p))
                .map_err(io::Error::other)?
                .ok_or_else(invalid)?;
            if !domains.contains(domain) || name.contains('.') {
                return Err(invalid());
            }
            let expected = domain.join(".lake/build/lib/lean").join(name).with_extension("olean");
            if selected != expected {
                return Err(invalid());
            }
            hash.update(file_hash(&domain.join(name).with_extension("lean"))?);
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
    // lead to another captured output; the first supported builtin edge is Init.
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
            .map(|(name, domain)| domain.join(".lake/build/lib/lean").join(name).with_extension("olean"))
            .collect();
        let search =
            std::env::join_paths(self.roots.iter().filter(|path| *path != &self.release)).map_err(io::Error::other)?;
        for (name, domain) in &self.modules {
            let mut cmd = Command::new(&lean);
            cmd.current_dir(&self.project)
                .env("LEAN_PATH", &search)
                .arg("--deps")
                .arg(domain.join(name).with_extension("lean"));
            let out = writer.run(&cmd, Duration::from_secs(30))?;
            if !out.status.success() {
                return Err(io::Error::other(String::from_utf8_lossy(&out.stderr).into_owned()));
            }
            for edge in std::str::from_utf8(&out.stdout).map_err(io::Error::other)?.lines() {
                let path = Path::new(edge);
                if !path.is_absolute() || (!local.iter().any(|p| p == path) && path != self.release.join("Init.olean"))
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
    /// Build the selected plugin-free default-layout project from clean output domains
    /// and capture the result before releasing the same writer.
    pub fn build_generation(
        self,
        writer: &mut lean_toolchain::shared_writer::SharedWriter,
    ) -> io::Result<PreparedGeneration> {
        if writer.project_root() != self.project || !writer.covers_project(&self.project)? {
            return Err(invalid());
        }
        let (target, domain) = self.modules.first().ok_or_else(invalid)?;
        if domain != &self.project || !target.bytes().all(|b| b.is_ascii_alphanumeric()) || target.is_empty() {
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
        let mut cmd = std::process::Command::new(lake);
        cmd.current_dir(&self.project)
            .env("LAKE_ARTIFACT_CACHE", "false")
            .args(["--no-cache", "build", target]);
        let built = writer.run(&cmd, std::time::Duration::from_secs(90))?;
        if !built.status.success() {
            return Err(io::Error::other(String::from_utf8_lossy(&built.stderr).into_owned()));
        }
        let identity = self.identity(writer.write_roots())?;
        self.check_edges(writer)?;
        Ok(PreparedGeneration { plan: self, identity })
    }
}
impl PreparedGeneration {
    /// Project selected by the parent when it bound this record to its configured primary.
    pub fn project(&self) -> &Path {
        &self.plan.project
    }

    /// Bounded launch data for an explicitly selected generation. Contains no module bytes.
    pub fn launch_input(&self) -> io::Result<String> {
        let input = serde_json::to_string(self)?;
        if input.len() > 4096 {
            return Err(invalid());
        }
        Ok(input)
    }
    /// Re-resolve under reader custody before any native import. Caller supplies the separately
    /// selected project, known release and installed shim roots. Keep the result through use.
    pub fn verify_before_import(
        input: &str,
        project: &Path,
        release: &Path,
        shims: &[PathBuf],
        ordered_inputs: &[PathBuf],
        names: &[String],
    ) -> io::Result<PreparedProject> {
        if input.len() > 4096 {
            return Err(invalid());
        }
        let generation: Self = serde_json::from_str(input)?;
        let plan = &generation.plan;
        let pre_import = plan.roots.iter().filter(|path| path.as_path() != release);
        if plan.project != project
            || plan.release != release
            || pre_import.ne(ordered_inputs.iter())
            || names
                .iter()
                .any(|name| name != "Init" && !plan.modules.iter().any(|(approved, _)| approved == name))
        {
            return Err(invalid());
        }
        let held = PreparedProject::capture(project, &plan.roots, shims, release)?;
        if plan.identity(&held.domains)? != generation.identity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "prepared module content changed",
            ));
        }
        Ok(held)
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
    let order = vec![dep_root.clone(), main_root.clone(), builtin.clone(), builtin.clone()];
    let input = fs::read_to_string(base.join("launch-input"))?;
    let names = vec![String::from("Root")];
    let verify = || PreparedGeneration::verify_before_import(&input, &project, &builtin, &[], &order[..2], &names);
    assert!(
        PreparedGeneration::verify_before_import(&input, &project, &builtin, &[], &order[..2], &["Other".into()])
            .is_err()
    );
    assert!(
        PreparedGeneration::verify_before_import(
            &input,
            &project,
            &builtin,
            &[],
            &[main_root.clone(), dep_root.clone()],
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
    assert!(prepared.check_observed(Some(&observed[..3])).is_err());
    let mut swapped = observed.clone();
    swapped.swap(0, 1);
    assert!(prepared.check_observed(Some(&swapped)).is_err());
    assert!(prepared.check_observed(None).is_err());
    assert!(prepared.check_observed(Some(&observed[1..])).is_err());
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
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/r2-prepared-project")
        .canonicalize()?;
    let project = base.join("project");
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
    fs::write(
        project.join("Root.lean"),
        "import Dep\ntheorem first_ok : True := dependency_ok\ntheorem second_ok : True := first_ok\n",
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
        let sidecar: serde_json::Value = serde_json::from_slice(&fs::read(path)?)?;
        let mut roots = vec![project.join(".lake/build/lib/lean"), dep.join(".lake/build/lib/lean")];
        for root in sidecar["deployment"]["prepared"]["shim_search_paths"]
            .as_array()
            .ok_or_else(invalid)?
        {
            roots.push(PathBuf::from(root.as_str().ok_or_else(invalid)?));
        }
        roots.push(release.clone());
        roots
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
    let incomplete = ImportPlan::new(
        project.clone(),
        plan.roots.clone(),
        plan.release.clone(),
        vec![("Root".into(), project.clone())],
    );
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
    fs::write(base.join(output), generation.launch_input()?)?;
    drop(writer);
    Ok(())
}

#[test]
#[ignore = "requires the disposable installed-worker control after its parent exits"]
fn collect_installed_fixture() -> io::Result<()> {
    use std::time::Duration;
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/r2-prepared-project")
        .canonicalize()?;
    let owners = base.join("owners");
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
