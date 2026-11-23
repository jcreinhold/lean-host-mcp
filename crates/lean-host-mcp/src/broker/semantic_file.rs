//! Supervised semantic adapter for an explicitly selected prepared project.
use super::verify_file::PreparedReader;
use super::{
    BrokerCall, ProjectBroker, ProjectHint, Result, RuntimeFacts, SemanticProofSearchRequest,
    SemanticProofSearchResult, ServerError, freshness_from_meta,
};
use crate::cancellation::CallCancellation;
use lean_semantic_search_runtime::SemanticSearchRuntimeProvenance;
use lean_toolchain::shared_writer::LocalArtifactRead;
use std::{
    fs,
    io::{self, Read as _},
    path::{Path, PathBuf},
    time::Duration,
};

fn invalid(error: impl std::fmt::Display) -> ServerError {
    ServerError::BadProject(error.to_string())
}

impl ProjectBroker {
    pub(super) async fn semantic_file(
        &self,
        hint: ProjectHint,
        imports: Vec<String>,
        freshness: Vec<String>,
        request: SemanticProofSearchRequest,
        input: Option<&str>,
    ) -> Result<BrokerCall<SemanticProofSearchResult>> {
        let root = self.native_root(&hint)?;
        let meta = self.meta(&root)?;
        let (reader, setup, activity) = if let Some(input) = input {
            let activity = self.preparation_activity(&root).await?;
            let mut selected = imports;
            selected.extend(request.candidate_modules.iter().cloned());
            (Self::prepared_reader(&root, &meta, input, &selected)?, None, activity)
        } else {
            let file = request
                .goal
                .file_label
                .as_deref()
                .ok_or_else(|| invalid("missing semantic source file"))?;
            let (setup, mut reader, activity) = self
                .file_reader_with_setup(&hint, &meta, file, &request.goal.source_text)
                .await?;
            reader.roots = crate::lake_meta::artifact_roots(&root);
            if reader.roots.iter().any(|p| !reader.custody.covers_artifact_root(p)) {
                return Err(invalid("semantic declaration root outside held project domains"));
            }
            reader.roots.push(reader.sysroot.join("lib/lean"));
            (reader, Some(setup), activity)
        };
        let PreparedReader {
            custody,
            sysroot,
            mut domains,
            shim_guards,
            roots,
            ..
        } = reader;
        let cancellation = CallCancellation::current();
        let label = meta.toolchain.clone();
        let prefix = sysroot.clone();
        let (activity, runtime) = tokio::task::spawn_blocking(move || {
            (
                activity,
                crate::project::semantic_runtime(&label, &prefix, &cancellation),
            )
        })
        .await
        .map_err(invalid)?;
        let runtime = runtime.map_err(invalid)?;
        let path = runtime
            .built
            .resolved_manifest_path()
            .map_err(invalid)?
            .canonicalize()?;
        let package = path
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == ".lake"))
            .and_then(Path::parent)
            .ok_or_else(|| invalid("missing semantic runtime package"))?
            .to_owned();
        // Dropping the build writer closes its input. Its observer can still be
        // reaping the helper when the completed build returns.
        let started = std::time::Instant::now();
        let budget = Duration::from_millis(self.request_timeout_millis());
        let runtime_guard = loop {
            match lean_toolchain::shared_writer::LocalArtifactRead::try_acquire(std::slice::from_ref(&package)) {
                Ok(guard) => break guard,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && started.elapsed() < budget => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => return Err(error.into()),
            }
        };
        let image = runtime.built.dylib_path().map_err(invalid)?.canonicalize()?;
        let bootstrap = select_semantic_runtime(
            &runtime_guard,
            &package,
            &path,
            &image,
            &sysroot,
            &meta.toolchain,
            RuntimeProvenance {
                built: Some(&runtime.provenance),
                pinned: None,
            },
        )?;
        let body = request.reader_input(&roots)?;
        domains.push(package);
        let bytes = crate::lake_options::reader_output(
            "Lean semantic feature reader",
            root,
            Duration::from_millis(self.request_timeout_millis()),
            self.config.max_projects.get(),
            custody,
            None,
            Some(Box::new((activity, shim_guards, runtime_guard))),
            move |assets| {
                let driver = assets.join("SemanticFeatures.lean");
                let entry = if let Some(setup) = setup {
                    fs::write(assets.join("module.setup.json"), setup.bytes)?;
                    include_str!("../lake/SemanticSetup.lean")
                } else {
                    include_str!("../lake/SemanticPrepared.lean")
                };
                fs::write(&driver, [include_str!("../lake/SemanticFeatures.lean"), entry].concat())?;
                let data = assets.join("semantic.json");
                fs::write(&data, body)?;
                Ok(crate::native_reader::NativeReaderProgram {
                    command: vec![
                        sysroot.join("bin/lean").into_os_string(),
                        "--run".into(),
                        driver.into_os_string(),
                        data.into_os_string(),
                    ],
                    environment: vec![
                        (
                            "LEAN_PATH".into(),
                            std::env::join_paths([bootstrap]).map_err(io::Error::other)?,
                        ),
                        ("LEAN_SYSROOT".into(), sysroot.into_os_string()),
                    ],
                    cwd: assets.into(),
                    artifact_roots: domains,
                })
            },
        )
        .await
        .map_err(|e| e.error)?;
        Ok(BrokerCall {
            value: request.reader_result(&bytes)?,
            runtime: RuntimeFacts::default(),
            freshness: freshness_from_meta(&meta, freshness),
        })
    }
}

/// Ordinary calls retain their builder provenance. An admitted reader supplies
/// an independent digest of the exact installed files through its intent.
#[derive(Clone, Copy)]
struct RuntimeProvenance<'a> {
    built: Option<&'a SemanticSearchRuntimeProvenance>,
    pinned: Option<&'a [u8; 32]>,
}

fn select_semantic_runtime(
    custody: &LocalArtifactRead,
    package: &Path,
    path: &Path,
    image: &Path,
    sysroot: &Path,
    toolchain: &str,
    approval: RuntimeProvenance<'_>,
) -> Result<PathBuf> {
    custody.validate()?;
    let pinned_bytes = if let Some(expected) = approval.pinned {
        let selected = snapshot_semantic_runtime(package, path, image)?;
        if selected.digest != *expected {
            return Err(invalid("approved semantic runtime artifacts changed"));
        }
        Some(selected)
    } else {
        None
    };
    let (manifest, provenance) = match (&pinned_bytes, approval.built) {
        (Some(snapshot), None) => (
            lean_toolchain::CapabilityManifest::from_bytes(path, &snapshot.manifest).map_err(invalid)?,
            std::borrow::Cow::Owned(serde_json::from_slice(&snapshot.provenance).map_err(invalid)?),
        ),
        (None, Some(provenance)) => (
            lean_toolchain::CapabilityManifest::read(path).map_err(invalid)?,
            std::borrow::Cow::Borrowed(provenance),
        ),
        _ => return Err(invalid("unexpected semantic runtime authorization")),
    };
    if !image.starts_with(package)
        || image != manifest.primary_dylib
        || manifest.module != lean_semantic_search_runtime::LIBRARY_NAME
        || manifest.package != provenance.materialized_package
        || provenance.runtime_source_digest != lean_semantic_search_runtime::RUNTIME_SOURCE_DIGEST
        || provenance.source_revision != lean_semantic_search_runtime::SOURCE_REVISION
        || provenance.toolchain_label != toolchain
    {
        return Err(invalid("semantic runtime selection changed"));
    }
    let bootstrap = semantic_bootstrap(image)?;
    if !bootstrap.join("LeanSemanticSearch/Capability.olean").is_file() {
        return Err(invalid("missing compiled semantic capability"));
    }
    if let Some(snapshot) = pinned_bytes {
        let raw: serde_json::Value = serde_json::from_slice(&snapshot.manifest).map_err(invalid)?;
        if !manifest.dependencies.is_empty()
            || raw
                .pointer("/build_toolchain/sysroot")
                .and_then(serde_json::Value::as_str)
                != sysroot.to_str()
            || manifest.lean_header_sha256.as_deref() != Some(crate::toolchain::hash_lean_header(sysroot)?.as_str())
        {
            return Err(invalid("approved semantic runtime toolchain or dependencies changed"));
        }
    }
    custody.validate()?;
    Ok(bootstrap)
}

fn semantic_bootstrap(image: &Path) -> Result<PathBuf> {
    Ok(image
        .parent()
        .ok_or_else(|| invalid("missing semantic image directory"))?
        .join("lean")
        .canonicalize()?)
}

fn read_bounded_semantic_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
    if path.canonicalize()? != path || !fs::symlink_metadata(path)?.is_file() {
        return Err(invalid("noncanonical semantic runtime input"));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("semantic runtime input exceeds its bound"));
    }
    Ok(bytes)
}

struct SemanticRuntimeSnapshot {
    digest: [u8; 32],
    manifest: Vec<u8>,
    provenance: Vec<u8>,
}

fn snapshot_semantic_runtime(package: &Path, manifest: &Path, image: &Path) -> Result<SemanticRuntimeSnapshot> {
    use sha2::{Digest as _, Sha256};
    let bootstrap = semantic_bootstrap(image)?;
    let sidecar = package.join("semantic-search-runtime.json");
    if package.canonicalize()? != package
        || !manifest.starts_with(package.join(".lake/lean-rs-build-cache"))
        || !image.starts_with(package.join(".lake/build/lib"))
        || bootstrap != package.join(".lake/build/lib/lean")
    {
        return Err(invalid("semantic runtime files escape their approved package layout"));
    }

    let manifest_bytes = read_bounded_semantic_file(manifest, 1024 * 1024)?;
    let provenance = read_bounded_semantic_file(&sidecar, 16 * 1024)?;
    let mut directories = vec![bootstrap];
    let mut files = vec![manifest.to_path_buf(), image.to_path_buf(), sidecar.clone()];
    let mut entries_seen = 0usize;
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            entries_seen = entries_seen.saturating_add(1);
            if entries_seen > 512 || entry.file_type()?.is_symlink() {
                return Err(invalid("unsupported semantic runtime module tree"));
            }
            if entry.file_type()?.is_dir() {
                directories.push(entry.path());
            } else if entry.file_type()?.is_file() {
                let name = entry.file_name();
                let name = name.to_str().ok_or_else(|| invalid("non-UTF8 semantic artifact"))?;
                if [".olean", ".olean.private", ".olean.server", ".ir", ".ir.sig"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix))
                {
                    files.push(entry.path());
                }
            } else {
                return Err(invalid("unsupported semantic runtime artifact node"));
            }
        }
    }
    files.sort_unstable();
    let mut digest = Sha256::new();
    let mut remaining = 64 * 1024 * 1024u64;
    for file in files {
        let relative = file
            .strip_prefix(package)
            .map_err(invalid)?
            .to_str()
            .ok_or_else(|| invalid("non-UTF8 semantic artifact path"))?;
        let captured;
        let bytes = if file == manifest {
            manifest_bytes.as_slice()
        } else if file == sidecar {
            provenance.as_slice()
        } else {
            captured = read_bounded_semantic_file(&file, remaining)?;
            captured.as_slice()
        };
        remaining = remaining
            .checked_sub(bytes.len() as u64)
            .ok_or_else(|| invalid("semantic runtime exceeds its capture bound"))?;
        digest.update(relative.as_bytes());
        digest.update([0]);
        digest.update(Sha256::digest(bytes));
    }
    Ok(SemanticRuntimeSnapshot {
        digest: digest.finalize().into(),
        manifest: manifest_bytes,
        provenance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an existing provisioned semantic runtime; never builds it"]
    fn qualified_semantic_runtime_selects_exact_prebuilt_files_or_refuses() -> Result<()> {
        let path = PathBuf::from(
            std::env::var_os("R2_SEMANTIC_PREBUILT_MANIFEST")
                .ok_or_else(|| invalid("set R2_SEMANTIC_PREBUILT_MANIFEST"))?,
        )
        .canonicalize()?;
        let package = path
            .ancestors()
            .find(|p| p.file_name().is_some_and(|n| n == ".lake"))
            .and_then(Path::parent)
            .ok_or_else(|| invalid("missing fixture package"))?;
        let guard = LocalArtifactRead::try_acquire(&[package.to_owned()])?;
        let bytes = read_bounded_semantic_file(&path, 1024 * 1024)?;
        let snapshot: serde_json::Value = serde_json::from_slice(&bytes).map_err(invalid)?;
        let sysroot = snapshot
            .pointer("/build_toolchain/sysroot")
            .and_then(serde_json::Value::as_str)
            .map(Path::new)
            .ok_or_else(|| invalid("missing fixture sysroot"))?;
        let manifest = lean_toolchain::CapabilityManifest::from_bytes(&path, &bytes).map_err(invalid)?;
        let sidecar = package.join("semantic-search-runtime.json");
        let provenance: SemanticSearchRuntimeProvenance =
            serde_json::from_slice(&read_bounded_semantic_file(&sidecar, 16 * 1024)?).map_err(invalid)?;
        let image = &manifest.primary_dylib;
        let bootstrap = semantic_bootstrap(image)?;
        let expected = snapshot_semantic_runtime(package, &path, image)?.digest;
        let select = |path: &Path, expected| {
            select_semantic_runtime(
                &guard,
                package,
                path,
                image,
                sysroot,
                &provenance.toolchain_label,
                RuntimeProvenance {
                    built: None,
                    pinned: Some(expected),
                },
            )
        };
        assert_eq!(select(&path, &expected)?, bootstrap);
        let wrong = expected.map(|byte| !byte);
        assert!(select(&path, &wrong).is_err());
        assert!(select(&path.with_extension("absent.json"), &expected).is_err());
        assert_eq!(snapshot_semantic_runtime(package, &path, image)?.digest, expected);
        Ok(())
    }
}
