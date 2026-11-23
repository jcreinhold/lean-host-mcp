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
    for row in include_str!("shim-inputs-0.10.6.txt").lines() {
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
        ensure!(actual == hash, "published shim input changed: {name}");
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
    // Bind every compiled package module, including split olean data if emitted.
    // Package order in the search path is part of the installation contract.
    for row in include_str!("shim-inputs-0.10.6.txt").lines() {
        let (_, name) = row.split_once(' ').context("invalid packaged shim inventory")?;
        let (package, file) = name.split_once('/').context("missing shim package")?;
        let Some(module) = file.strip_suffix(".lean").filter(|name| *name != "lakefile") else {
            continue;
        };
        let root = match package {
            "lean-rs-interop-shims" => search.first(),
            "lean-rs-host-shims" => search.get(1),
            _ => None,
        }
        .context("missing installed module root")?;
        let leaf = root.join(module).with_extension("olean");
        ensure!(
            leaf.canonicalize()? == leaf && leaf.is_file(),
            "invalid installed module {module}"
        );
        paths.push(leaf.clone());
        for suffix in ["olean.private", "olean.server", "ir"] {
            let companion = leaf.with_extension(suffix);
            match fs::symlink_metadata(&companion) {
                Ok(meta) => {
                    ensure!(
                        meta.is_file() && companion.canonicalize()? == companion,
                        "invalid installed module companion"
                    );
                    paths.push(companion);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
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
    let mut runtime = InstalledRuntime {
        sysroot: sysroot.into(),
        prepared,
        files,
    };
    runtime.map_generation_paths(|path| Ok(path.strip_prefix(candidate).map_err(std::io::Error::other)?.to_owned()))?;
    // The SDK manifest was generated in this final directory. Preserve its exact bytes.
    record.bind_runtime(runtime)?;
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
        .lifetime_directory(candidate.join("probe-owners"))
        .lifetime_socket_directory(crate::native_socket::directory()?)
        .checked_native_loader_environment()
        .prepared_host_artifacts(prepared);
    // A Mac installer must leave a durable generation the SDK can collect after shutdown.
    #[cfg(target_os = "macos")]
    let child = child.native_guardian(probe_guardian()?);
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
        #[cfg(lean_host_mcp_development_warm)]
        let version_matches = handle.runtime_metadata().worker_version == crate::toolchain::WARM_WORKER_VERSION;
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
        #[cfg(lean_host_mcp_development_warm)]
        ensure!(
            version_matches,
            "installed worker handshake lacks the exact warm SDK version"
        );
        #[cfg(all(test, target_os = "macos"))]
        images?;
        ensure!(
            matches!(result,LeanWorkerDeclarationInspectionResult::Found{declaration} if declaration.name=="Nat.add_zero"),
            "missing actual declaration"
        );
        Ok(())
    };
    let result = run();
    #[cfg(target_os = "macos")]
    {
        let owners = candidate.join("probe-owners");
        lean_toolchain::native_guardian::collect_closed_owner_records(&owners, Duration::from_secs(5)).with_context(
            || match &result {
                Ok(()) => "installer probe returned; closure unconfirmed".to_owned(),
                Err(error) => format!("installer probe failed ({error:#}); closure unconfirmed"),
            },
        )?;
    }
    result
}

#[cfg(target_os = "macos")]
fn probe_guardian() -> Result<std::path::PathBuf> {
    // Unit-test executables have a libtest entry point instead of the host CLI.
    #[cfg(test)]
    return Ok(
        std::path::PathBuf::from(std::env::var_os("R2_HOST").context("set R2_HOST to the matching host CLI")?)
            .canonicalize()?,
    );
    #[cfg(not(test))]
    Ok(std::env::current_exe()?.canonicalize()?)
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
    let mut record = bind(candidate, id, sysroot, prepared)?;
    // Keep both exclusive domains through the installer's image inventory and sidecar publication.
    drop(custody);
    let snapshot = crate::toolchain::WorkerSidecar::load(candidate)
        .and_then(crate::toolchain::WorkerSidecar::into_runtime)
        .context("unreadable installed snapshot")?;
    probe(candidate, id, sysroot, snapshot.prepared)?;
    record.complete(
        &super::install_worker::sha256_prefix(&candidate.join(crate::toolchain::WORKER_FILE_NAME), 64)?,
        crate::smoke::SmokeOutcome::Passed,
    )?;
    #[cfg(lean_host_mcp_development_warm)]
    record.qualify_warm()?;
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
            .and_then(crate::toolchain::WorkerSidecar::into_runtime)
            .context("missing installed runtime")?;
        probe(&generation, &id, &id.elan_dir()?.canonicalize()?, runtime.prepared)
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a disposable installed generation and matching R2_HOST"]
    fn rejected_probe_retires_its_generation() -> Result<()> {
        let generation =
            std::path::PathBuf::from(std::env::var_os("R2_GENERATION").context("missing disposable generation")?);
        let id = crate::toolchain::ToolchainId::parse("v4.36.0")?;
        let sysroot = id.elan_dir()?.canonicalize()?;
        let runtime = crate::toolchain::WorkerSidecar::load(&generation)
            .and_then(crate::toolchain::WorkerSidecar::into_runtime)
            .context("missing installed runtime")?;
        let shadow = generation.join("smoke-project/.lake/build/lib/lean/Init");
        fs::create_dir(&shadow)?;
        let refusal = probe(&generation, &id, &sysroot, runtime.prepared);
        fs::remove_dir(&shadow)?;
        let Err(error) = refusal else {
            anyhow::bail!("shadow must refuse the actual smoke request");
        };
        ensure!(
            !format!("{error:#}").contains("closure unconfirmed"),
            "cleanup failed: {error:#}"
        );
        let owners = generation.join("probe-owners");
        lean_toolchain::native_guardian::collect_closed_owner_records(&owners, std::time::Duration::from_secs(5))?;
        ensure!(
            fs::read_dir(owners)?.all(|e| e.is_ok_and(|e| e.file_name() == ".admission.lock")),
            "probe did not retire"
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
            super::super::install_worker::prepare_generation(
                &slot,
                &id,
                &id.elan_dir()?.canonicalize()?,
                &env_path("R2_SHIM_SOURCE")?,
                &env_path("R2_WRITER")?,
                |dir| {
                    fs::copy(env_path("R2_WORKER")?, dir.join(crate::toolchain::WORKER_FILE_NAME))?;
                    Ok(())
                },
            )
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
        let result = refuse("earlier import root shadows builtin Init");
        fs::remove_dir(shadow)?;
        result?;
        Ok(())
    }
}
