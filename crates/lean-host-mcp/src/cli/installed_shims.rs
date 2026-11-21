//! Build the two coupled SDK shim packages in a fresh installer-owned directory.
use anyhow::{Context as _, Result, ensure};
use lean_rs_worker_protocol::types::LeanWorkerPreparedHostArtifacts;
use sha2::{Digest as _, Sha256};
use std::{fs, path::Path};

pub(super) fn prepare(
    source: &Path,
    destination: &Path,
    sysroot: &Path,
    writer: &Path,
) -> Result<(
    LeanWorkerPreparedHostArtifacts,
    lean_toolchain::shared_writer::SharedWriter,
)> {
    fs::create_dir(destination)?;
    for row in include_str!("shim-inputs-0.10.1.txt").lines() {
        let (hash, name) = row.split_once(' ').context("invalid packaged shim inventory")?;
        let path = source.join("shims").join(name);
        ensure!(
            path.canonicalize()? == path && path.metadata()?.len() <= 1024 * 1024,
            "invalid packaged shim source {name}"
        );
        let bytes = fs::read(path)?;
        let mut actual = String::with_capacity(64);
        for byte in Sha256::digest(&bytes) {
            use std::fmt::Write as _;
            write!(actual, "{byte:02x}")?;
        }
        // SDK fb98113 changes only this Lean source in the inventoried shim inputs.
        let setup_shim = name == "lean-rs-host-shims/LeanRsHostShims/InfoTree.lean"
            && actual == "f6e5aa7d41520e906b9251a634fe0195a26bf1ff30dccf1b9f567ac0cb7aa591";
        ensure!(actual == hash || setup_shim, "published shim input changed: {name}");
        let output = destination.join(name);
        fs::create_dir_all(output.parent().context("missing source directory")?)?;
        fs::write(output, bytes)?;
    }
    let host = destination.join("lean-rs-host-shims");
    let interop = destination.join("lean-rs-interop-shims");
    let mut builder = lean_toolchain::CargoLeanCapability::new(&host, "LeanRsHostShims")
        .package("lean_rs_host_shims")
        .module("LeanRsHostShims")
        .lean_sysroot(sysroot)
        .writer_executable(writer)
        .lean_num_threads(2);
    if let Some(key) = lean_toolchain::generated_output_key_for_sysroot(sysroot)? {
        builder = builder.generated_output_key(key);
    }
    for signature in lean_toolchain::host_shim_abi::compiled_host_binding_signatures().into_values() {
        builder = builder.export_signature(signature);
    }
    let mut custody = lean_toolchain::shared_writer::SharedWriter::open_with_write_roots(
        writer,
        &host,
        std::slice::from_ref(&interop),
        std::time::Duration::from_secs(30),
        &|| false,
    )?;
    let built = builder.build_quiet_with_writer(&mut custody, || false)?;
    let manifest = built.manifest_path().canonicalize()?;
    let json = fs::read_to_string(&manifest)?;
    let parsed = lean_toolchain::CapabilityManifest::from_bytes(&manifest, json.as_bytes())
        .map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    let [dep] = parsed.dependencies.as_slice() else {
        anyhow::bail!("expected the sole interop dependency")
    };
    ensure!(
        dep.module_initializer()
            .is_some_and(|i| i.module_name() == "LeanRsInterop" && i.package_name() == "lean_rs_interop_shims"),
        "unexpected interop initializer"
    );
    let image = dep.path_ref().canonicalize()?;
    let primary = built.dylib_path().canonicalize()?;
    ensure!(
        image.starts_with(&interop) && primary.starts_with(&host) && manifest.starts_with(&host),
        "shim escaped installation"
    );
    let text = |p: &Path| -> Result<String> { Ok(p.to_str().context("non-UTF8 install path")?.to_owned()) };
    let roots = lean_toolchain::shared_writer::project_artifact_root_union(&[host, interop])?;
    Ok((
        LeanWorkerPreparedHostArtifacts {
            protocol: 1,
            shim_manifest_path: text(&manifest)?,
            shim_manifest_json: json,
            artifact_roots: roots.iter().map(|p| text(p)).collect::<Result<_>>()?,
            shim_search_paths: [&image, &primary]
                .iter()
                .map(|p| {
                    text(
                        &p.parent()
                            .context("missing library directory")?
                            .join("lean")
                            .canonicalize()?,
                    )
                })
                .collect::<Result<_>>()?,
        },
        custody,
    ))
}

pub(super) fn bind(
    candidate: &Path,
    id: &crate::toolchain::ToolchainId,
    sysroot: &Path,
    prepared: LeanWorkerPreparedHostArtifacts,
) -> Result<crate::toolchain::WorkerSidecar> {
    use crate::toolchain::{InstalledRuntime, WORKER_FILE_NAME, WorkerSidecar, hash_lean_header};
    let project = candidate.join("smoke-project");
    fs::create_dir(&project)?;
    fs::create_dir_all(project.join(".lake/build/lib/lean"))?;
    // Initialize the smoke project's canonical writer domain before worker admission.
    let _lock = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(lean_toolchain::shared_writer::project_lock_path(&project)?)?;
    let manifest = Path::new(&prepared.shim_manifest_path);
    let data = lean_toolchain::CapabilityManifest::from_bytes(manifest, prepared.shim_manifest_json.as_bytes())
        .map_err(|e| anyhow::anyhow!("{}", e.message()))?;
    let mut paths = vec![manifest.to_owned(), data.primary_dylib];
    paths.extend(data.dependencies.iter().map(|dep| dep.path_ref().to_owned()));
    for rel in ["include/lean/lean.h", "include/lean/version.h", "lib/lean/Init.olean"] {
        paths.push(sysroot.join(rel));
    }
    for file in fs::read_dir(sysroot.join("lib/lean"))? {
        let file = file?.path();
        if file.extension().is_some_and(|ext| ext == "dylib" || ext == "so") {
            paths.push(file.canonicalize()?);
        }
    }
    let search: Vec<_> = prepared
        .shim_search_paths
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
    for module in crate::toolchain::VERIFIER_MODULES {
        let artifact = lean_toolchain::olean_candidate(&search, module, |path| fs::metadata(path))?
            .context("verification driver shim module missing")?;
        ensure!(
            artifact.canonicalize()? == artifact,
            "aliased verification driver module"
        );
        paths.push(artifact);
    }
    let files = paths
        .into_iter()
        .map(|p| Ok((p.clone(), super::install_worker::sha256_prefix(&p, 64)?)))
        .collect::<Result<_>>()?;

    let mut record = WorkerSidecar::candidate(
        id,
        hash_lean_header(sysroot)?,
        Some(super::install_worker::sha256_prefix(
            &candidate.join(WORKER_FILE_NAME),
            64,
        )?),
    );
    record.bind_runtime(InstalledRuntime {
        sysroot: sysroot.into(),
        prepared,
        files,
    })?;
    record.write(candidate)?;
    Ok(record)
}

pub(super) fn probe(
    candidate: &Path,
    id: &crate::toolchain::ToolchainId,
    sysroot: &Path,
    prepared: LeanWorkerPreparedHostArtifacts,
) -> Result<()> {
    use lean_rs_worker_parent::{
        LeanWorkerChild, LeanWorkerDeclarationInspectionRequest, LeanWorkerHostHandleBuilder, LeanWorkerProcessTracker,
    };
    use lean_rs_worker_protocol::types::LeanWorkerDeclarationInspectionResult;
    use std::time::Duration;
    let worker = candidate.join(crate::toolchain::WORKER_FILE_NAME);
    let tracker = LeanWorkerProcessTracker::new();
    let child = LeanWorkerChild::for_toolchain(&worker, sysroot)
        .checked_native_loader_environment()
        .prepared_host_artifacts(prepared);
    let mut handle =
        LeanWorkerHostHandleBuilder::shims_only(candidate.join("smoke-project"), std::iter::empty::<String>())
            .require_host_runtime(
                crate::runtime_compatibility::session_expectation(id.as_str(), None).map_err(anyhow::Error::msg)?,
            )
            .worker_child(child)
            .process_tracker(tracker.clone())
            .num_threads(2)
            .startup_timeout(Duration::from_secs(30))
            .request_timeout(Duration::from_secs(20))
            .open()?;
    let result = handle.inspect_declaration_with_imports(
        vec!["Init".to_owned()],
        &LeanWorkerDeclarationInspectionRequest::new("Nat.add_zero"),
        None,
        None,
    );
    let cleanup = handle.try_shutdown();
    ensure!(
        tracker.active_processes() == 0,
        "actual worker must retire: {cleanup:?}"
    );
    let result = result?;
    cleanup?;
    ensure!(
        matches!(result,LeanWorkerDeclarationInspectionResult::Found{declaration} if declaration.name=="Nat.add_zero"),
        "missing actual declaration"
    );
    Ok(())
}

/// Prepare and qualify a worker in its final candidate directory. Absolute image
/// paths in the generated manifest make this directory part of its identity.
pub(super) fn qualify(
    candidate: &Path,
    id: &crate::toolchain::ToolchainId,
    source: &Path,
    sysroot: &Path,
    writer: &Path,
) -> Result<()> {
    let mut entries = fs::read_dir(candidate)?;
    let worker = entries.next().transpose()?.context("missing staged worker")?;
    ensure!(
        worker.file_name() == crate::toolchain::WORKER_FILE_NAME
            && worker.file_type()?.is_file()
            && entries.next().is_none(),
        "expected a fresh candidate containing only its worker executable"
    );
    let (prepared, custody) = prepare(source, &candidate.join("shims"), sysroot, writer)?;
    let mut record = bind(candidate, id, sysroot, prepared.clone())?;
    // Keep both exclusive domains through the installer's image inventory and sidecar publication.
    drop(custody);
    probe(candidate, id, sysroot, prepared)?;
    record.complete(
        &super::install_worker::sha256_prefix(&candidate.join(crate::toolchain::WORKER_FILE_NAME), 64)?,
        crate::smoke::SmokeOutcome::Passed,
    )?;
    record.write(candidate)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires fresh candidate and matched published inputs"]
    fn installed_init_candidate_and_refusals() -> Result<()> {
        let env_path = |name| {
            std::env::var_os(name)
                .map(std::path::PathBuf::from)
                .context("missing candidate input")
        };
        let slot = env_path("R2_CANDIDATE")?;
        let id = crate::toolchain::ToolchainId::parse("v4.36.0")?;
        let candidate = super::super::install_worker::prepare_generation(
            &slot,
            &id,
            &id.elan_dir()?.canonicalize()?,
            &env_path("R2_SHIM_SOURCE")?,
            &env_path("R2_WRITER")?,
            |dir| {
                fs::copy(env_path("R2_WORKER")?, dir.join(crate::toolchain::WORKER_FILE_NAME))?;
                Ok(())
            },
        )?;
        ensure!(
            !slot.join("current-generation").exists(),
            "qualification unexpectedly activated a worker"
        );
        let worker = candidate.join(crate::toolchain::WORKER_FILE_NAME);
        let sidecar = candidate.join("worker.json");
        let original = fs::read(&sidecar)?;
        let mut value: serde_json::Value = serde_json::from_slice(&original)?;
        ensure!(
            value.pointer("/smoke/result").and_then(serde_json::Value::as_str) == Some("passed"),
            "sidecar lost successful smoke"
        );
        let refuse = |why: &str| -> Result<()> {
            let out = std::process::Command::new(&worker)
                .arg("--lean-rs-ready-host-artifacts")
                .output()?;
            ensure!(
                !out.status.success() && out.stdout.is_empty() && String::from_utf8_lossy(&out.stderr).contains(why),
                "wrong refusal: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            Ok(())
        };
        *value
            .pointer_mut("/deployment/worker_sha256")
            .context("worker digest")? = serde_json::Value::String("0".repeat(64));
        fs::write(&sidecar, serde_json::to_vec(&value)?)?;
        let result = refuse("installed worker digest mismatch");
        fs::write(&sidecar, &original)?;
        result?;
        let mut value: serde_json::Value = serde_json::from_slice(&original)?;
        value
            .pointer_mut("/deployment/prepared/shim_search_paths")
            .context("prepared shim paths")?
            .as_array_mut()
            .context("ordered paths")?
            .reverse();
        fs::write(&sidecar, serde_json::to_vec(&value)?)?;
        let result = refuse("ordered shim roots differ");
        fs::write(&sidecar, &original)?;
        result?;
        let shadow = candidate.join("smoke-project/.lake/build/lib/lean/Init");
        fs::create_dir(&shadow)?;
        let result = refuse("earlier import root shadows builtin Init");
        fs::remove_dir(shadow)?;
        result?;
        Ok(())
    }
}
