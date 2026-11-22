//! Admission of the two operator-selected semantic calls for one companion lifetime.
use super::authority::{digest, invalid};
use lean_host_mcp_worker::prepared_project::PreparedGeneration;
use lean_rs::module::{AuthorizedWorkerSnapshot, WorkerExportContract};
use lean_rs::{LeanResult, LeanRuntime};
use lean_rs_worker_child::WorkerStringCall;
use lean_rs_worker_protocol::worker_exports::WorkerExportOperation;
use lean_toolchain::shared_writer::{LocalArtifactRead, project_artifact_roots};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

const EXPORTS: [&str; 2] = [
    "lean_semantic_search_proof_goal_features",
    "lean_semantic_search_declaration_features",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    pub(super) semantic_v1: Policy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    prepared: PreparedGeneration,
    generation: PathBuf,
    sidecar_sha256: String,
    sysroot: PathBuf,
    artifact_root: PathBuf,
    manifest: Asset,
    image: Asset,
    dependencies: Vec<Asset>,
    commands: [Command; 2],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    path: PathBuf,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    operation: String,
    export: String,
    payload_sha256: String,
}

pub(super) struct Semantic {
    policy: Policy,
    snapshot: Vec<u8>,
    custody: Arc<LocalArtifactRead>,
    next: AtomicUsize,
}

fn matches_digest(bytes: &[u8], expected: &str) -> bool {
    expected.len() == 64
        && expected
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && expected
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .zip(Sha256::digest(bytes))
            .all(|(digits, byte)| {
                std::str::from_utf8(digits)
                    .ok()
                    .and_then(|s| u8::from_str_radix(s, 16).ok())
                    == Some(byte)
            })
}

impl Semantic {
    pub(super) fn open(
        policy: Policy,
        generation: &Path,
        sysroot: &Path,
        sidecar: &[u8],
    ) -> io::Result<(String, Self)> {
        let reject = || invalid("semantic launch selection differs from installed deployment");
        if !matches_digest(sidecar, &policy.sidecar_sha256)
            || policy.generation != generation
            || policy.sysroot != sysroot
            || !policy.dependencies.is_empty()
            || policy.artifact_root.canonicalize()? != policy.artifact_root
            || policy.commands.iter().zip(EXPORTS).any(|(c, export)| {
                c.operation != "json_command"
                    || c.export != export
                    || c.payload_sha256.len() != 64
                    || !c
                        .payload_sha256
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        {
            return Err(reject());
        }
        let prepared = policy.prepared.launch_input()?;
        let domains = project_artifact_roots(policy.prepared.project())?;
        let mut all = domains.clone();
        all.push(policy.artifact_root.clone());
        let custody = Arc::new(LocalArtifactRead::try_acquire(&all)?);
        if domains != project_artifact_roots(policy.prepared.project())? {
            return Err(reject());
        }
        PreparedGeneration::check_identity(&prepared, &domains)?;
        let manifest = &policy.manifest.path;
        if !manifest.starts_with(policy.artifact_root.join(".lake/lean-rs-build-cache"))
            || !policy
                .image
                .path
                .starts_with(policy.artifact_root.join(".lake/build/lib"))
            || manifest.canonicalize()? != *manifest
            || fs::metadata(manifest)?.len() > 1024 * 1024
            || digest(&policy.image.path)? != policy.image.sha256
        {
            return Err(reject());
        }
        let snapshot = fs::read(manifest)?;
        if !matches_digest(&snapshot, &policy.manifest.sha256) {
            return Err(reject());
        }
        let m = lean_toolchain::CapabilityManifest::from_bytes(manifest, &snapshot).map_err(|_| reject())?;
        let raw: serde_json::Value = serde_json::from_slice(&snapshot)?;
        if m.package != "lean_semantic_search"
            || m.module != "LeanSemanticSearch"
            || !m.dependencies.is_empty()
            || m.primary_dylib != policy.image.path
            || raw.pointer("/build_toolchain/sysroot").and_then(|x| x.as_str()) != sysroot.to_str()
            || m.lean_header_sha256.as_deref() != Some(digest(&sysroot.join("include/lean/lean.h"))?.as_str())
        {
            return Err(reject());
        }
        custody.validate()?;
        Ok((
            prepared,
            Self {
                policy,
                snapshot,
                custody,
                next: AtomicUsize::new(0),
            },
        ))
    }
    #[allow(
        unsafe_code,
        reason = "only the launch-approved 0.7.9 semantic image, initializer and two reviewed String IO exports are admitted under retained custody"
    )]
    pub(super) fn snapshot(
        &self,
        runtime: &'static LeanRuntime,
        path: &Path,
        bytes: &[u8],
    ) -> LeanResult<AuthorizedWorkerSnapshot<'static>> {
        if path != self.policy.manifest.path || bytes != self.snapshot || self.custody.validate().is_err() {
            return Err(super::authority::deny());
        }
        // SAFETY: startup policy pins the qualified ABI and image digest independently of the request.
        // The exact checked manifest has no explicit dependencies. The installed Elan runtime and
        // package read guards cover loading and use. Each String invocation is checked separately.
        Ok(unsafe {
            AuthorizedWorkerSnapshot::attest(
                runtime,
                path.into(),
                bytes.into(),
                EXPORTS
                    .into_iter()
                    .map(|s| (s.into(), WorkerExportContract::StringIoString))
                    .collect(),
                Arc::clone(&self.custody),
            )
        })
    }
    pub(super) fn command(&self, call: &WorkerStringCall<'_>) -> LeanResult<()> {
        let deny = super::authority::deny;
        let i = self.next.load(Ordering::Acquire);
        let c = self.policy.commands.get(i).ok_or_else(deny)?;
        if call.operation != WorkerExportOperation::JsonCommand
            || Path::new(call.project_root) != self.policy.prepared.project()
            || call.export != c.export
            || !call
                .capability
                .matches_snapshot(&self.policy.manifest.path, &self.snapshot)
            || !matches_digest(call.request_json.as_bytes(), &c.payload_sha256)
            || self.custody.validate().is_err()
        {
            return Err(deny());
        }
        self.next
            .compare_exchange(
                i,
                i.checked_add(1).ok_or_else(deny)?,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| deny())?;
        Ok(())
    }
}
