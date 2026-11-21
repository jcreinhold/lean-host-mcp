//! Test-only preflight. No native grant or loader entry point.
use lean_rs::host_bridge::{
    HostSourceOptions,
    module_requests::{DeclarationVerificationBatchRequest as Batch, DeclarationVerificationTarget as Target},
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

struct Approval {
    project: PathBuf,
    generation: PathBuf,
    worker_hash: [u8; 32],
    sidecar_hash: [u8; 32],
    batch: Batch,
    options: HostSourceOptions,
    // Operator-selected import order and the cooperating domain protecting each root.
    roots: Vec<(PathBuf, PathBuf)>,
}

fn refusal() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "verification preflight refused")
}
fn canonical(p: &Path) -> io::Result<()> {
    if p.canonicalize()?.as_os_str() != p.as_os_str() {
        return Err(refusal());
    }
    Ok(())
}
fn held(domain: &Path) -> io::Result<File> {
    canonical(domain)?;
    let p = lean_toolchain::shared_writer::project_lock_path(domain)?;
    let file = File::open(&p)?;
    file.try_lock_shared().map_err(io::Error::from)?;
    let a = file.metadata()?;
    let b = fs::symlink_metadata(p)?;
    if !b.is_file() || a.nlink() != 1 || (a.dev(), a.ino()) != (b.dev(), b.ino()) {
        return Err(refusal());
    }
    Ok(file)
}
fn regular(p: &Path) -> io::Result<File> {
    canonical(p)?;
    let f = File::open(p)?;
    let m = f.metadata()?;
    if !m.is_file() || m.nlink() != 1 {
        return Err(refusal());
    }
    Ok(f)
}
fn hash(p: &Path) -> io::Result<[u8; 32]> {
    let mut h = Sha256::new();
    let mut file = regular(p)?;
    let mut buffer = [0u8; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        h.update(buffer.get(..count).ok_or_else(refusal)?);
    }
    Ok(h.finalize().into())
}
impl Approval {
    // A versioned, length-delimited JSON tuple binds the existing typed preflight
    // without transporting source text or inventing a persistent approval file.
    fn launch_input(&self, operator_project: &Path) -> io::Result<String> {
        canonical(operator_project)?;
        if operator_project != self.project {
            return Err(refusal());
        }
        let [entry] = self.batch.targets.as_slice() else {
            return Err(refusal());
        };
        let Target::Name { name } = &entry.target else {
            return Err(refusal());
        };
        let source = Sha256::digest(self.batch.source.as_bytes());
        let opts = &self.options;
        let bytes = serde_json::to_vec(&(
            "lean-host-mcp/verify-batch/1",
            &self.project,
            &self.generation,
            self.worker_hash,
            self.sidecar_hash,
            &self.roots,
            source.as_slice(),
            (
                &entry.id,
                name,
                matches!(
                    self.batch.sorry_policy,
                    lean_rs::host_bridge::module_requests::SorryPolicy::Deny
                ),
                self.batch.report_axioms,
                self.batch.budgets.per_field_bytes,
                self.batch.budgets.total_bytes,
            ),
            (
                &opts.file,
                &opts.namespace,
                &opts.options_json,
                opts.heartbeats,
                opts.diagnostic_bytes,
            ),
        ))?;
        use std::fmt::Write as _;
        let mut input = String::from("verify-batch-v1:");
        for byte in Sha256::digest(bytes) {
            write!(&mut input, "{byte:02x}").map_err(io::Error::other)?;
        }
        Ok(input)
    }

    fn check(
        &self,
        launch_input: Option<&str>,
        worker: &Path,
        batch: &Batch,
        options: &HostSourceOptions,
        observed: Option<&[PathBuf]>,
    ) -> io::Result<Vec<File>> {
        if launch_input != Some(self.launch_input(&self.project)?.as_str()) {
            return Err(refusal());
        }
        let [entry] = self.batch.targets.as_slice() else {
            return Err(refusal());
        };
        if entry.id.is_empty()
            || !matches!(&entry.target,Target::Name{name} if !name.is_empty())
            || batch != &self.batch
            || batch.source.len() > 1024 * 1024
            || options.file != self.options.file
            || options.namespace != self.options.namespace
            || options.options_json != self.options.options_json
            || options.heartbeats != self.options.heartbeats
            || options.diagnostic_bytes != self.options.diagnostic_bytes
        {
            return Err(refusal());
        }
        let domains: BTreeSet<_> = [&self.project, &self.generation]
            .into_iter()
            .chain(self.roots.iter().map(|(_, d)| d))
            .collect();
        let guards = domains.into_iter().map(|d| held(d)).collect::<io::Result<Vec<_>>>()?;
        let roots = observed.ok_or_else(refusal)?;
        if roots.is_empty() || roots.len() != self.roots.len() {
            return Err(refusal());
        }
        for (actual, (expected, domain)) in roots.iter().zip(&self.roots) {
            canonical(expected)?;
            if !expected.is_dir() || !expected.starts_with(domain) || actual.as_os_str() != expected.as_os_str() {
                return Err(refusal());
            }
        }
        if worker != self.generation.join("lean-host-mcp-worker")
            || hash(worker)? != self.worker_hash
            || hash(&self.generation.join("worker.json"))? != self.sidecar_hash
        {
            return Err(refusal());
        }
        let path = Path::new(&options.file);
        if !path.starts_with(&self.project) || path.extension().and_then(|s| s.to_str()) != Some("lean") {
            return Err(refusal());
        }
        let mut bytes = Vec::new();
        regular(path)?
            .take((batch.source.len() as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes != batch.source.as_bytes() {
            return Err(refusal());
        }
        Ok(guards)
    }
}

#[test]
fn independent_batch_approval() -> io::Result<()> {
    check_handoff(|explicit| Ok(explicit.to_owned()))
}

pub(super) fn check_handoff(deliver: impl FnOnce(&str) -> io::Result<String>) -> io::Result<()> {
    use lean_rs::host_bridge::module_requests::{DeclarationVerificationBatchItem, SorryPolicy};
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .ok_or_else(refusal)?
        .canonicalize()?;
    let target = checkout.join("target");
    fs::create_dir_all(&target)?;
    canonical(&target)?;
    let scratch = target.join(format!("r2-preflight-{}", std::process::id()));
    fs::create_dir(&scratch)?;
    let project = scratch.join("project");
    let generation = scratch.join("generation");
    let release = scratch.join("release");
    for d in [&project, &generation, &release] {
        fs::create_dir(d)?;
        File::create_new(lean_toolchain::shared_writer::project_lock_path(d)?)?;
    }
    let source = "import Init\ntheorem checked : True := True.intro\n";
    let file = project.join("Example.lean");
    fs::write(&file, source)?;
    let worker = generation.join("lean-host-mcp-worker");
    fs::write(&worker, b"test worker")?;
    let sidecar = generation.join("worker.json");
    fs::write(&sidecar, b"test sidecar")?;
    let batch = Batch {
        source: source.into(),
        targets: vec![DeclarationVerificationBatchItem {
            id: "one".into(),
            target: Target::Name { name: "checked".into() },
        }],
        sorry_policy: SorryPolicy::Deny,
        report_axioms: true,
        budgets: lean_rs::host_bridge::module_requests::ModuleQueryOutputBudgets::default(),
    };
    let options = HostSourceOptions {
        file: file.display().to_string(),
        namespace: String::new(),
        options_json: "[]".into(),
        heartbeats: 1000,
        diagnostic_bytes: 1024,
    };
    let roots = vec![project.clone(), release.clone()];
    let policy = Approval {
        project: project.clone(),
        generation,
        worker_hash: hash(&worker)?,
        sidecar_hash: hash(&sidecar)?,
        batch: batch.clone(),
        options: options.clone(),
        roots: roots.iter().map(|p| (p.clone(), p.clone())).collect(),
    };
    let selected = policy.launch_input(&project)?;
    let launch_input = deliver(&selected)?;
    assert_eq!(launch_input, selected);
    assert!(policy.launch_input(&release).is_err());
    assert!(policy.check(None, &worker, &batch, &options, Some(&roots)).is_err());
    assert!(
        policy
            .check(Some("verify-batch-v1:stale"), &worker, &batch, &options, Some(&roots))
            .is_err()
    );
    let check =
        |b: &Batch, o: &HostSourceOptions, r: Option<&[PathBuf]>| policy.check(Some(&launch_input), &worker, b, o, r);
    let guards = check(&batch, &options, Some(&roots))?;
    let lock = File::open(lean_toolchain::shared_writer::project_lock_path(&project)?)?;
    assert!(lock.try_lock().is_err());
    drop(guards);
    lock.try_lock().map_err(io::Error::from)?;
    assert!(check(&batch, &options, Some(&roots)).is_err());
    lock.unlock()?;
    assert!(check(&batch, &options, None).is_err());
    let mut swapped = roots.clone();
    swapped.reverse();
    assert!(check(&batch, &options, Some(&swapped)).is_err());
    let mut opt = options.clone();
    opt.heartbeats = opt.heartbeats.saturating_add(1);
    assert!(check(&batch, &opt, Some(&roots)).is_err());
    let mut changed = batch.clone();
    changed.source.push_str("-- changed\n");
    fs::write(&file, &changed.source)?;
    assert!(check(&changed, &options, Some(&roots)).is_err());
    assert!(check(&batch, &options, Some(&roots)).is_err());
    let outside = scratch.join("outside.lean");
    fs::write(&outside, source)?;
    fs::remove_file(&file)?;
    std::os::unix::fs::symlink(outside, &file)?;
    assert!(check(&batch, &options, Some(&roots)).is_err());
    fs::remove_file(&file)?;
    fs::write(&file, source)?;
    changed = batch.clone();
    changed.targets.clear();
    assert!(check(&changed, &options, Some(&roots)).is_err());
    for change_id in [true, false] {
        let mut request = batch.clone();
        let [item] = request.targets.as_mut_slice() else {
            return Err(refusal());
        };
        if change_id {
            item.id = "different-id".into();
        } else {
            item.target = Target::Name {
                name: "different_theorem".into(),
            };
        }
        assert!(check(&request, &options, Some(&roots)).is_err());
    }
    let release_lock = lean_toolchain::shared_writer::project_lock_path(&release)?;
    fs::remove_file(&release_lock)?;
    assert!(check(&batch, &options, Some(&roots)).is_err());
    File::create_new(release_lock)?;
    let alternate = scratch.join("same-worker-bytes");
    fs::copy(&worker, &alternate)?;
    assert!(
        policy
            .check(Some(&launch_input), &alternate, &batch, &options, Some(&roots))
            .is_err()
    );
    drop(check(&batch, &options, Some(&roots))?);
    fs::write(sidecar, b"different generation record")?;
    assert!(check(&batch, &options, Some(&roots)).is_err());
    fs::remove_dir_all(&scratch)?;
    assert!(!scratch.exists());
    Ok(())
}
