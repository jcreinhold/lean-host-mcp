//! One committed writer command owns Lake setup and its setup-aware Lean consumer.
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    LakeProjectMeta,
    broker::{module_setup, verify_file::VerifierShims},
    native_store::store::AdmissionDeadline,
};

pub(crate) const OPERATION: &str = "lean_registered_module_verify_v1";
pub(crate) const DIAGNOSTICS: &str = "lean_file_diagnostics_v1";
pub(crate) const OUTLINE: &str = "lean_file_outline_v1";

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum Projection {
    Verify,
    Diagnostics,
    Outline,
}

impl Projection {
    pub(crate) fn from_operation(operation: &str) -> Option<Self> {
        match operation {
            OPERATION => Some(Self::Verify),
            DIAGNOSTICS => Some(Self::Diagnostics),
            OUTLINE => Some(Self::Outline),
            _ => None,
        }
    }
}

const MAX_RESULT: usize = 60 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub(crate) file: PathBuf,
    pub(crate) sha256: [u8; 32],
    #[serde(default)]
    pub(crate) declarations: Vec<String>,
    pub(crate) prepared_sha256: Option<[u8; 32]>,
}

impl Request {
    pub(crate) fn valid(&self) -> bool {
        crate::managed_prepared::readiness_writer::configured_consumer::SourceIdentity {
            file: self.file.clone(),
            sha256: self.sha256,
        }
        .valid()
            && self.declarations.len() <= 16
            && self.declarations.iter().collect::<BTreeSet<_>>().len() == self.declarations.len()
            && self.declarations.iter().all(|name| {
                name.len() <= 256
                    && name.split('.').all(|part| {
                        let mut chars = part.chars();
                        chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
                            && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '\''))
                    })
            })
    }

    pub(crate) fn matches_projection(&self, projection: Projection) -> bool {
        self.declarations.is_empty() == (projection != Projection::Verify)
    }

    pub(crate) fn matches_prepared(&self, prepared: Option<&str>) -> bool {
        self.prepared_sha256 == prepared.map(|input| Sha256::digest(input.as_bytes()).into())
    }

    pub(crate) fn receipt(&self, projection: Projection, output: &std::process::Output) -> Option<serde_json::Value> {
        use lean_rs_worker_protocol::types::{
            LeanWorkerDeclarationVerificationBatchResult as Outcome, LeanWorkerDeclarationVerificationTarget as Target,
        };
        if !output.status.success() || output.stdout.len() > MAX_RESULT {
            return None;
        }
        if projection != Projection::Verify {
            let mut body: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
            body.as_object_mut()?.insert(
                "facts".into(),
                serde_json::to_value(lean_rs_worker_parent::LeanWorkerModuleQueryCacheFacts::uncached(
                    output.stdout.len() as u64,
                ))
                .ok()?,
            );
            serde_json::from_value::<DiagnosticOutcome>(body.clone()).ok()?;
            let receipt = serde_json::json!({"source_sha256": self.sha256, "outcome": body});
            return (receipt.to_string().len() <= MAX_RESULT).then_some(receipt);
        }
        let outcome: Outcome = serde_json::from_slice(&output.stdout).ok()?;
        match &outcome {
            Outcome::Ok { results, .. } | Outcome::MissingImports { results, .. } => {
                if results.len() != self.declarations.len()
                    || results
                        .iter()
                        .zip(&self.declarations)
                        .enumerate()
                        .any(|(index, (row, name))| {
                            row.id != index.to_string()
                                || !matches!(&row.target, Target::Name { name: actual } if actual == name)
                        })
                {
                    return None;
                }
            }
            _ => (),
        }
        serde_json::from_slice(&output.stdout).ok()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub(crate) root: PathBuf,
    pub(crate) worker: PathBuf,
    pub(crate) domains: Vec<PathBuf>,
    pub(crate) prepared: Option<String>,
    pub(crate) request: Request,
    pub(crate) projection: Projection,
    pub(crate) boot: [u8; 16],
    pub(crate) expires_ns: u64,
}

pub(crate) fn capture(command: &mut Command, limit: usize) -> io::Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing capture pipe"))?;
    let mut bytes = Vec::new();
    let read = (&mut stdout).take(limit as u64 + 1).read_to_end(&mut bytes);
    if read.is_err() || bytes.len() > limit {
        let _killed = child.kill();
    }
    drop(stdout);
    let status = child.wait()?;
    read?;
    if !status.success() || bytes.len() > limit {
        return Err(io::Error::other(
            "setup verification command failed or exceeded capture limit",
        ));
    }
    Ok(bytes)
}

fn read_source(root: &Path, request: &Request) -> io::Result<String> {
    let path = root.join(&request.file);
    let file = fs::File::from(rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?);
    if path.canonicalize()? != path || !file.metadata()?.is_file() {
        return Err(io::Error::other("invalid setup verification source"));
    }
    let mut bytes = Vec::new();
    file.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 * 1024 || Sha256::digest(&bytes).as_slice() != request.sha256 {
        return Err(io::Error::other("setup verification source changed"));
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}

/// Run only in the existing writer's committed command process group.
///
/// # Errors
/// Refuses invalid ownership, changed input, expired admission, or incomplete setup/result.
pub fn run(encoded: &str) -> io::Result<()> {
    if encoded.len() > 24 * 1024 {
        return Err(io::Error::other("setup verification plan exceeds limit"));
    }
    let plan: Plan = serde_json::from_str(encoded)?;
    if !plan.request.valid()
        || !plan.request.matches_projection(plan.projection)
        || !plan.request.matches_prepared(plan.prepared.as_deref())
        || plan.root.canonicalize()? != plan.root
        || std::env::current_dir()? != plan.root
    {
        return Err(io::Error::other("invalid setup verification plan"));
    }
    let deadline = AdmissionDeadline {
        boot: plan.boot,
        expires_ns: plan.expires_ns,
    };
    let authorize = |domains: &[PathBuf]| -> io::Result<()> {
        for root in domains {
            crate::shared_writer::verify_committed_writer_process(&lean_toolchain::shared_writer::project_lock_path(
                root,
            )?)?;
        }
        deadline.remaining().map_err(io::Error::other)?;
        Ok(())
    };
    authorize(std::slice::from_ref(&plan.root))?;
    let shims = VerifierShims::select(&LakeProjectMeta::from_explicit(&plan.root).map_err(io::Error::other)?)
        .map_err(io::Error::other)?;
    if shims.worker != plan.worker {
        return Err(io::Error::other("installed verifier generation changed"));
    }
    let project_domains = lean_toolchain::shared_writer::project_artifact_roots(&plan.root)?;
    if !project_domains
        .iter()
        .chain(&shims.domains)
        .all(|domain| plan.domains.contains(domain))
    {
        return Err(io::Error::other("verification domain is outside admitted custody"));
    }
    authorize(&plan.domains)?;
    if let Some(prepared) = &plan.prepared {
        lean_host_mcp_worker::prepared_project::PreparedGeneration::check_identity(prepared, &project_domains)?;
    }
    shims.validate().map_err(io::Error::other)?;
    let source = read_source(&plan.root, &plan.request)?;
    let path = plan.root.join(&plan.request.file);
    let assets = tempfile::Builder::new().prefix("setup-verify-").tempdir()?;
    let mut setup_command = Command::new(shims.sysroot.join("bin/lake"));
    setup_command
        .current_dir(&plan.root)
        .args(["--no-build", "--no-cache", "setup-file"])
        .arg(&path)
        .env("LAKE_CACHE_DIR", assets.path())
        .env("LAKE_ARTIFACT_CACHE", "false")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_NO_LAZY_FETCH", "1");
    authorize(&plan.domains)?;
    let bytes = capture(&mut setup_command, crate::shared_writer::MODULE_SETUP_LIMIT)?;
    let setup = module_setup::stage(bytes, &project_domains, &shims.sysroot, &plan.root, &path)?;
    let targets = plan
        .request
        .declarations
        .iter()
        .enumerate()
        .map(|(i, name)| serde_json::json!({"id":i.to_string(),"name":name}))
        .collect::<Vec<_>>();
    let input = serde_json::json!({
        "source": source,"file":path,"roots":[],"imports":[],"targets":targets,
        "allowSorry":false,"reportAxioms":true,"perFieldBytes":4096,"totalBytes":32768,
        "namespaceContext":"","optionsJson":lean_toolchain::lean_options_json(&setup.options),
        "heartbeats":200000,"diagnosticBytes":16384
    });
    let entry = match plan.projection {
        Projection::Diagnostics => concat!(
            include_str!("../lake/VerifyCommon.lean"),
            "private abbrev projectFile := fileDiagnosticsWithSetup\n",
            include_str!("../lake/OutlineSetup.lean")
        ),
        Projection::Outline => concat!(
            include_str!("../lake/VerifyCommon.lean"),
            "private abbrev projectFile := declarationOutlineWithSetup\n",
            include_str!("../lake/OutlineSetup.lean")
        ),
        Projection::Verify => concat!(
            include_str!("../lake/VerifyCommon.lean"),
            "private abbrev verifySetup := verifyDeclarationBatchWithSetup\n",
            include_str!("../lake/VerifySetup.lean")
        ),
    };
    let driver = assets.path().join("FileDriver.lean");
    let data = assets.path().join("request.json");
    fs::write(assets.path().join("module.setup.json"), &setup.bytes)?;
    fs::write(&driver, entry)?;
    fs::write(&data, serde_json::to_vec(&input)?)?;
    setup.check_exclusive(&lean_toolchain::shared_writer::project_artifact_roots(&plan.root)?)?;
    if let Some(prepared) = &plan.prepared {
        lean_host_mcp_worker::prepared_project::PreparedGeneration::check_identity(prepared, &project_domains)?;
    }
    shims.validate().map_err(io::Error::other)?;
    read_source(&plan.root, &plan.request)?;
    authorize(&plan.domains)?;
    let mut lean = Command::new(shims.sysroot.join("bin/lean"));
    lean.current_dir(assets.path())
        .args(["-j1", "--run"])
        .arg(driver)
        .arg(data)
        .env(
            "LEAN_PATH",
            std::env::join_paths(&shims.paths).map_err(io::Error::other)?,
        )
        .env("LEAN_SYSROOT", &shims.sysroot);
    let verdict = capture(&mut lean, MAX_RESULT)?;
    authorize(&plan.domains)?;
    use io::Write as _;
    io::stdout().lock().write_all(&verdict)
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reference {
    key: crate::native_store::store::NativeKey,
    capability: [u8; 32],
}

pub(crate) struct ManagedCall<'a> {
    pub(crate) reference: Reference,
    pub(crate) store: &'a std::sync::Arc<parking_lot::Mutex<crate::native_store::store::NativeStore>>,
}

pub(crate) type DiagnosticOutcome = lean_rs_worker_parent::LeanWorkerModuleQueryBatchOutcome;

pub(crate) type Outcome = lean_rs_worker_protocol::types::LeanWorkerDeclarationVerificationBatchResult;

impl ManagedCall<'_> {
    pub(crate) async fn verify(
        &self,
        broker: &crate::ProjectBroker,
        hint: &crate::ProjectHint,
        root: &Path,
        file: &Path,
        imports: Vec<String>,
        request: lean_rs_worker_protocol::types::LeanWorkerDeclarationVerificationBatchRequest,
    ) -> crate::Result<crate::broker::BrokerCall<Outcome>> {
        use lean_rs_worker_protocol::types::LeanWorkerDeclarationVerificationTarget as Target;
        let unavailable = |message: &str| crate::ServerError::BadProject(message.to_owned());
        let names = request
            .targets
            .iter()
            .map(|target| match &target.target {
                Target::Name { name } => Ok(name.as_str()),
                _ => Err(unavailable("managed verification requires named declarations")),
            })
            .collect::<crate::Result<Vec<_>>>()?;
        let body = self
            .saved(broker, root, file, &request.source, &names, Projection::Verify)
            .await?;
        let mut value: Outcome =
            serde_json::from_slice(&body).map_err(|_| unavailable("invalid managed verification result"))?;
        match &mut value {
            Outcome::Ok { results, .. } | Outcome::MissingImports { results, .. } => {
                if results.len() != request.targets.len() {
                    return Err(unavailable("incomplete managed verification result"));
                }
                for (row, original) in results.iter_mut().zip(request.targets) {
                    if row.target != original.target {
                        return Err(unavailable("managed verification result target changed"));
                    }
                    row.id = original.id;
                }
            }
            _ => (),
        }
        let identity = broker.project_identity_without_worker(hint, imports)?;
        Ok(crate::broker::BrokerCall {
            value,
            runtime: identity.runtime,
            freshness: identity.freshness,
        })
    }
    pub(crate) async fn project_file(
        &self,
        broker: &crate::ProjectBroker,
        hint: &crate::ProjectHint,
        root: &Path,
        file: &Path,
        source: &str,
        imports: Vec<String>,
        projection: Projection,
    ) -> crate::Result<crate::broker::CachedBrokerCall<DiagnosticOutcome>> {
        #[derive(Deserialize)]
        struct Receipt {
            source_sha256: [u8; 32],
            outcome: DiagnosticOutcome,
        }
        let body = self.saved(broker, root, file, source, &[], projection).await?;
        let receipt: Receipt = serde_json::from_slice(&body)
            .map_err(|_| crate::ServerError::BadProject("invalid managed file projection result".into()))?;
        if Sha256::digest(source.as_bytes()).as_slice() != receipt.source_sha256 {
            return Err(crate::ServerError::BadProject(
                "managed file projection source digest changed".into(),
            ));
        }
        let identity = broker.project_identity_without_worker(hint, imports)?;
        Ok(crate::broker::CachedBrokerCall {
            value: receipt.outcome,
            freshness: identity.freshness,
            runtime: identity.runtime,
            freshly_processed: false,
        })
    }

    async fn saved(
        &self,
        broker: &crate::ProjectBroker,
        root: &Path,
        file: &Path,
        source: &str,
        names: &[&str],
        projection: Projection,
    ) -> crate::Result<Vec<u8>> {
        use crate::native_store::store::{Capability, Gate, ResourceReport, ResultKnowledge};
        let unavailable = |message: &str| crate::ServerError::BadProject(message.to_owned());
        let key = self.reference.key;
        let capability = Capability(self.reference.capability);
        let snapshot = self
            .store
            .lock()
            .client_get(key, &capability)
            .map_err(|_| unavailable("managed verification identity unavailable"))?;
        let (approved_root, admitted, approved_projection) =
            crate::managed_prepared::readiness_writer::setup_intent(&snapshot.intent)
                .ok_or_else(|| unavailable("managed verification intent unavailable"))?;
        let digest: [u8; 32] = Sha256::digest(source.as_bytes()).into();
        if approved_projection != projection
            || approved_root != root
            || file.strip_prefix(root).ok() != Some(admitted.file.as_path())
            || digest != admitted.sha256
            || !admitted
                .declarations
                .iter()
                .map(String::as_str)
                .eq(names.iter().copied())
        {
            return Err(unavailable(
                "managed verification differs from the admitted source or targets",
            ));
        }
        let saved = if snapshot.gate == Gate::Pending && snapshot.scope.is_none() {
            crate::managed_prepared::start(broker, self.store, key, capability.clone())
                .await
                .map_err(|error| unavailable(&error.message))?;
            self.store
                .lock()
                .client_get(key, &capability)
                .map_err(|_| unavailable("managed verification result unavailable"))?
        } else {
            snapshot
        };
        if saved.resource_report != ResourceReport::Closed {
            return Err(unavailable("managed verification custody is unresolved"));
        }
        match saved.result {
            ResultKnowledge::Retained { body, .. } | ResultKnowledge::Acknowledged { body, .. } => Ok(body),
            _ => Err(unavailable("managed setup result unavailable")),
        }
    }
}
