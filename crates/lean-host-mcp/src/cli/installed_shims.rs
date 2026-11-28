//! Give a candidate worker its own built copy of the SDK shims, record their
//! digests, and probe the result.
use anyhow::{Context as _, Result, ensure};
use lean_rs_worker_protocol::types::LeanWorkerPreparedHostArtifacts;
use std::{ffi::OsStr, fs, path::Path, time::Duration};

/// How long the worker's preparation mode may take to build both shim packages.
const PREPARE_TIMEOUT: Duration = Duration::from_mins(30);

/// Files Lake writes for a compiled module that the worker may load: its
/// `.olean`, the split data beside it, and its IR.
const MODULE_SUFFIXES: [&str; 4] = [".olean", ".olean.private", ".olean.server", ".ir"];

/// Run the candidate's worker in its preparation mode: it writes the shim
/// sources it was built with to `<candidate>/shims`, builds them there with
/// plain Lake, and prints their descriptor.
fn prepare(candidate: &Path, sysroot: &Path) -> Result<LeanWorkerPreparedHostArtifacts> {
    let destination = candidate.join(lean_host_mcp_worker::SHIMS_DIR);
    let flags = [
        OsStr::new(crate::host_preparation::PREPARE_FLAG),
        destination.as_os_str(),
    ];
    crate::host_preparation::run_artifact_mode(
        &candidate.join(crate::toolchain::WORKER_FILE_NAME),
        sysroot,
        &flags,
        PREPARE_TIMEOUT,
        &|| false,
    )?
    .context("the worker's preparation mode reported no shims")
}

pub(super) fn bind(
    candidate: &Path,
    id: &crate::toolchain::ToolchainId,
    sysroot: &Path,
    prepared: LeanWorkerPreparedHostArtifacts,
) -> Result<crate::toolchain::WorkerSidecar> {
    use crate::toolchain::{WORKER_FILE_NAME, WorkerSidecar, hash_lean_header};
    let project = candidate.join("smoke-project");
    fs::create_dir(&project)?;
    fs::create_dir_all(project.join(".lake/build/lib/lean"))?;
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
    // Record every compiled shim module, so the worker refuses a changed one.
    for root in &prepared.shim_search_paths {
        for entry in walkdir::WalkDir::new(root) {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy();
            if !MODULE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
                continue;
            }
            ensure!(
                entry.file_type().is_file(),
                "invalid installed module {}",
                entry.path().display()
            );
            paths.push(entry.into_path());
        }
    }
    let files = paths
        .into_iter()
        .map(|p| Ok((p.clone(), super::install_worker::sha256_prefix(&p, 64)?)))
        .collect::<Result<_>>()?;

    let mut deployment = lean_host_mcp_worker::Deployment {
        worker_sha256: super::install_worker::sha256_prefix(&candidate.join(WORKER_FILE_NAME), 64)?,
        sysroot: sysroot.into(),
        prepared,
        files,
    };
    // The SDK manifest was generated in this final directory. Preserve its exact bytes.
    deployment.relative_to(candidate)?;
    let record = WorkerSidecar::new(id, hash_lean_header(sysroot)?, Some(deployment));
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
    let worker = candidate.join(crate::toolchain::WORKER_FILE_NAME);
    let tracker = LeanWorkerProcessTracker::new();
    let child = LeanWorkerChild::for_toolchain(&worker, sysroot)
        .checked_native_loader_environment()
        .prepared_host_artifacts(prepared);
    let run = || -> Result<()> {
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
        #[cfg(all(test, target_os = "macos"))]
        let images = if std::env::var_os("R2_GENERATION").is_some() {
            tests::check_loaded_images(candidate, &worker)
        } else {
            Ok(())
        };
        let cleanup = handle.try_shutdown();
        ensure!(
            tracker.active_processes() == 0,
            "actual worker must retire: {cleanup:?}"
        );
        let result = result?;
        cleanup?;
        #[cfg(all(test, target_os = "macos"))]
        images?;
        ensure!(
            matches!(result,LeanWorkerDeclarationInspectionResult::Found{declaration} if declaration.name=="Nat.add_zero"),
            "missing actual declaration"
        );
        Ok(())
    };
    run()
}

/// Prepare and qualify a worker in its final candidate directory. Absolute image
/// paths in the generated manifest make this directory part of its identity.
pub(super) fn qualify(candidate: &Path, id: &crate::toolchain::ToolchainId, sysroot: &Path) -> Result<()> {
    let mut entries = fs::read_dir(candidate)?;
    let worker = entries.next().transpose()?.context("missing staged worker")?;
    ensure!(
        worker.file_name() == crate::toolchain::WORKER_FILE_NAME
            && worker.file_type()?.is_file()
            && entries.next().is_none(),
        "expected a fresh candidate containing only its worker executable"
    );
    let prepared = prepare(candidate, sysroot)?;
    let mut record = bind(candidate, id, sysroot, prepared)?;
    let snapshot = crate::toolchain::WorkerSidecar::load(candidate)
        .and_then(crate::toolchain::WorkerSidecar::into_deployment)
        .context("unreadable installed snapshot")?;
    probe(candidate, id, sysroot, snapshot.prepared)?;
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
    #[cfg(target_os = "macos")]
    pub(super) fn check_loaded_images(candidate: &Path, worker: &Path) -> Result<()> {
        let children = std::process::Command::new("/usr/bin/pgrep")
            .args(["-P", &std::process::id().to_string()])
            .output()?;
        ensure!(children.status.success(), "no worker child for map inspection");
        let pids = std::str::from_utf8(&children.stdout)?
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(",");
        let maps = std::process::Command::new("/usr/sbin/lsof")
            .args(["-nP", "-a", "-p", &pids, "-d", "txt", "-Fn"])
            .output()?;
        ensure!(maps.status.success(), "worker mapping inspection failed");
        let text = std::str::from_utf8(&maps.stdout)?;
        let mapped: Vec<_> = text
            .lines()
            .filter_map(|l| l.strip_prefix('n'))
            .map(Path::new)
            .collect();
        ensure!(mapped.contains(&worker), "installed executable missing from child maps");
        for package in ["host__shims", "interop__shims"] {
            let images: Vec<_> = mapped
                .iter()
                .filter(|p| {
                    p.extension().is_some_and(|e| e == "dylib")
                        && p.file_name().is_some_and(|n| n.to_string_lossy().contains(package))
                })
                .collect();
            ensure!(
                matches!(images.as_slice(), [image] if image.starts_with(candidate.join("shims"))),
                "unexpected mapped {package} image: {images:?}"
            );
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "run inside a child-only sandbox denying checkout and Cargo source reads"]
    fn isolated_installed_init() -> Result<()> {
        for var in ["R2_HOST_CANARY", "R2_SDK_CANARY", "R2_CARGO_CANARY"] {
            let path = std::env::var_os(var).context(var)?;
            ensure!(
                fs::File::open(path).is_err_and(|err| err.kind() == std::io::ErrorKind::PermissionDenied),
                "expected sandbox denial for {var}"
            );
        }
        let generation = std::path::PathBuf::from(std::env::var_os("R2_GENERATION").context("missing generation")?);
        let id = crate::toolchain::ToolchainId::parse("v4.36.0")?;
        let runtime = crate::toolchain::WorkerSidecar::load(&generation)
            .and_then(crate::toolchain::WorkerSidecar::into_deployment)
            .context("missing installed runtime")?;
        probe(&generation, &id, &id.elan_dir()?.canonicalize()?, runtime.prepared)
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a disposable installed generation"]
    fn rejected_probe_retires_its_generation() -> Result<()> {
        let generation =
            std::path::PathBuf::from(std::env::var_os("R2_GENERATION").context("missing disposable generation")?);
        let id = crate::toolchain::ToolchainId::parse("v4.36.0")?;
        let sysroot = id.elan_dir()?.canonicalize()?;
        let runtime = crate::toolchain::WorkerSidecar::load(&generation)
            .and_then(crate::toolchain::WorkerSidecar::into_deployment)
            .context("missing installed runtime")?;
        let shadow = generation.join("smoke-project/.lake/build/lib/lean/Init");
        fs::create_dir(&shadow)?;
        let refusal = probe(&generation, &id, &sysroot, runtime.prepared);
        fs::remove_dir(&shadow)?;
        let Err(error) = refusal else {
            anyhow::bail!("shadow must refuse the actual smoke request");
        };
        ensure!(
            !format!("{error:#}").contains("actual worker must retire"),
            "the probe's worker did not retire: {error:#}"
        );
        Ok(())
    }

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
        let prepare = || {
            super::super::install_worker::prepare_generation(&slot, &id, &id.elan_dir()?.canonicalize()?, |dir| {
                fs::copy(env_path("R2_WORKER")?, dir.join(crate::toolchain::WORKER_FILE_NAME))?;
                Ok(())
            })
        };
        let previous = prepare()?;
        let selector = slot.join("current-generation");
        let prior = previous
            .file_name()
            .context("missing generation name")?
            .as_encoded_bytes();
        use std::io::Write as _;
        fs::File::create_new(&selector)?.write_all(prior)?;
        let candidate = prepare()?;
        ensure!(
            fs::read(&selector)? == prior,
            "preparation changed the selected generation"
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
        let relative = Path::new(
            value
                .pointer("/deployment/prepared/shim_manifest_path")
                .and_then(serde_json::Value::as_str)
                .context("missing sidecar manifest path")?,
        );
        ensure!(
            relative.is_relative(),
            "sidecar must locate the installed manifest relatively"
        );
        let ready = std::process::Command::new(&worker)
            .current_dir(&previous)
            .arg("--lean-rs-ready-host-artifacts")
            .output()?;
        ensure!(ready.status.success(), "installed worker readiness failed");
        let prepared: LeanWorkerPreparedHostArtifacts = serde_json::from_slice(&ready.stdout)?;
        ensure!(
            Path::new(&prepared.shim_manifest_path) == candidate.join(relative),
            "worker resolved the manifest outside its own generation"
        );
        let support =
            Path::new(prepared.shim_search_paths.first().context("missing interop root")?).join("LeanRsInterop.olean");
        ensure!(
            support.canonicalize()? == support && support.starts_with(&candidate),
            "unexpected support path"
        );
        let original_support = fs::read(&support)?;
        fs::remove_file(&support)?;
        let missing = refuse("NotFound");
        fs::write(&support, b"substituted test module")?;
        let substituted = refuse("installed file digest mismatch");
        fs::write(&support, original_support)?;
        missing?;
        substituted?;
        ensure!(
            fs::read(&selector)? == prior,
            "refused candidate replaced the selected generation"
        );
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
        let result = refuse("shadows compiled package Init");
        fs::remove_dir(shadow)?;
        result?;
        Ok(())
    }
}
