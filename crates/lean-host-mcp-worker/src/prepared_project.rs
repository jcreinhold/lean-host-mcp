//! Project/dependency custody and the separate observed-root check for worker admission.
use lean_toolchain::shared_writer::{LocalArtifactRead, project_artifact_roots};
use std::{
    fs::{self, File},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub(super) struct PreparedProject {
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
    pub(super) fn capture(
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
    pub(super) fn check_observed(&self, observed: Option<&[String]>) -> io::Result<()> {
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
    let prepared = PreparedProject::capture(&project, &order, &[], &builtin)?;
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
    let mut command = Command::new(&lake);
    command.args(["--no-cache", "build", "Root"]).current_dir(&project);
    let built = writer.run(&command, Duration::from_secs(90))?;
    if !built.status.success() {
        return Err(io::Error::other(String::from_utf8_lossy(&built.stderr).into_owned()));
    }
    drop(writer);
    Ok(())
}
