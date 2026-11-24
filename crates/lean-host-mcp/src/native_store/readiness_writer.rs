//! One fixed Lake cache-readiness stage with native writer custody.
use std::{
    io,
    os::unix::process::ExitStatusExt as _,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::Arc,
    time::Duration,
};

use lean_rs_worker_parent::LeanWorkerCancellationToken;
use lean_toolchain::shared_writer::{SharedWriter, WriterDisposition, WriterFailure, WriterStagePermit};
use parking_lot::Mutex;
use rmcp::ErrorData as McpError;
use serde::Deserialize;

use crate::{
    ProjectBroker,
    broker::ProjectHint,
    native_store::store::{
        AdmissionDeadline, Capability, NativeStore, ResourceReport, Snapshot, readiness_writer::ReadinessDecision,
    },
    writer_preparation::{self, Plan, Receipt, Target},
};

#[path = "configured_consumer.rs"]
pub(crate) mod configured_consumer;
pub(super) const CONSUMER: &str = "lake_configured_consumer_v1";
use crate::writer_setup_verification as setup;
use configured_consumer::SourceIdentity;
pub(super) use setup::{
    COMMAND_TRIAL as SETUP_TRIAL, DIAGNOSTICS as SETUP_DIAGNOSTICS, OPERATION as SETUP_VERIFICATION,
    OUTLINE as SETUP_OUTLINE, PROOF_POSITION as SETUP_PROOF_POSITION, PROOF_SEARCH as SETUP_PROOF_SEARCH,
    PROOF_STEP as SETUP_PROOF_STEP,
};
pub(super) use writer_preparation::first_party_clean::OPERATION as FIRST_PARTY_CLEAN;
pub(super) use writer_preparation::regroup::OPERATION as REGROUP;
pub(super) use writer_preparation::tooling_suites::OPERATION as TOOLING_SUITES;
pub(super) use writer_preparation::whole_lint::OPERATION as WHOLE_LINT;
#[path = "formatter_fix.rs"]
mod formatter_fix;
pub(super) const FORMAT_FIX: &str = "lean_formatter_fix_v1";

pub(super) const FORMAT_PREVIEW: &str = "lean_formatter_preview_v1";

pub(super) fn formatter_approval(root: &Path) -> Option<([u8; 32], bool)> {
    formatter_fix::Approval::select(root)
        .ok()
        .map(|pin| (pin.sha256, pin.preview))
}

enum Stage {
    Cache,
    Preparation(Target),
    Consumer(SourceIdentity),
    Formatter(formatter_fix::Request),
    FormatterPreview(formatter_fix::Request),
    SetupVerification(setup::Request, setup::Projection),
}

// A failure body is evidence about the writer exchange, not a stage verdict
// or a substitute for the exact helper closure observation.
fn writer_body(
    result: io::Result<Output>,
    receipt: impl FnOnce(Output) -> Option<serde_json::Value>,
) -> serde_json::Value {
    use serde_json::json;

    match result {
        Err(error) => {
            let disposition = WriterFailure::from_io(&error).map(|failure| match failure.disposition {
                WriterDisposition::NotStarted => "not_started",
                WriterDisposition::Stopped => "stopped",
                WriterDisposition::Unknown => "unknown",
            });
            json!({
                "version": 1, "stage": "readiness-writer", "status": "unavailable",
                "reason": "writer_error", "error_kind": format!("{:?}", error.kind()),
                "writer_disposition": disposition,
            })
        }
        Ok(output) => {
            let status = output.status;
            receipt(output).unwrap_or_else(|| {
                if status.success() {
                    json!({
                        "version": 1, "stage": "readiness-writer", "status": "unavailable",
                        "reason": "receipt_rejected",
                    })
                } else {
                    json!({
                        "version": 1, "stage": "readiness-writer", "status": "unavailable",
                        "reason": "writer_command_failed", "command_exit_code": status.code(),
                        "command_signal": status.signal(),
                    })
                }
            })
        }
    }
}

pub(super) const OPERATION: &str = "lake_mathlib_cache_readiness_v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    operation: String,
    job: Vec<u8>,
    revision: u64,
    project: PathBuf,
    target: Option<Target>,
    source: Option<SourceIdentity>,
    formatter: Option<formatter_fix::Request>,
    setup_verification: Option<setup::Request>,
}

pub(crate) fn setup_intent(bytes: &[u8]) -> Option<(PathBuf, setup::Request, setup::Projection)> {
    let intent: Intent = serde_json::from_slice(bytes).ok()?;
    let projection = setup::Projection::from_operation(&intent.operation)?;
    if intent.job.is_empty()
        || intent.revision == 0
        || intent.target.is_some()
        || intent.source.is_some()
        || intent.formatter.is_some()
    {
        return None;
    }
    let setup = intent.setup_verification.filter(setup::Request::valid)?;
    if !setup.matches_projection(projection) {
        return None;
    }
    Some((intent.project, setup, projection))
}

pub(super) fn is_operation(intent: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(intent).is_ok_and(|value| {
        matches!(
            value.get("operation").and_then(serde_json::Value::as_str),
            Some(
                OPERATION
                    | writer_preparation::OPERATION
                    | writer_preparation::VERIFIER_OPERATION
                    | writer_preparation::GROUPED_VERIFIER_OPERATION
                    | CONSUMER
                    | FORMAT_FIX
                    | FORMAT_PREVIEW
                    | SETUP_VERIFICATION
                    | SETUP_DIAGNOSTICS
                    | SETUP_OUTLINE
                    | SETUP_TRIAL
                    | SETUP_PROOF_POSITION
                    | SETUP_PROOF_STEP
                    | SETUP_PROOF_SEARCH
                    | TOOLING_SUITES
                    | REGROUP
                    | WHOLE_LINT
                    | FIRST_PARTY_CLEAN
            )
        )
    })
}

pub(super) fn selection() -> io::Result<PathBuf> {
    let path = std::env::var_os("LEAN_HOST_MCP_MANAGED_READINESS_WRITER")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("no opt-in standalone readiness writer"))?;
    if !path.is_absolute() || !path.is_file() || path.canonicalize()? != path {
        return Err(io::Error::other(
            "readiness helper must be an exact canonical executable",
        ));
    }
    Ok(path)
}

fn lake_command(root: &Path, assets: &Path) -> io::Result<Command> {
    let lake = writer_preparation::lake_executable(root)?;
    let mut command = Command::new(lake);
    command
        .current_dir(root)
        .args(["--no-build", "--no-cache", "build", "@mathlib/+Mathlib:olean"])
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("LAKE_CACHE_DIR", assets)
        .env("TMPDIR", assets);
    Ok(command)
}

fn parse_intent(intent: &[u8]) -> Option<(PathBuf, Stage)> {
    let request: Intent = serde_json::from_slice(intent).ok()?;
    let stage = match (
        request.operation.as_str(),
        request.target,
        request.source,
        request.formatter,
        request.setup_verification,
    ) {
        (OPERATION, None, None, None, None) => Stage::Cache,
        (writer_preparation::OPERATION, Some(target), None, None, None)
            if !matches!(
                target,
                Target::VerifierModules { .. }
                    | Target::VerifierGroups { .. }
                    | Target::Regroup(_)
                    | Target::ToolingSuites { .. }
                    | Target::WholeLint { .. }
                    | Target::FirstPartyClean { .. }
            ) && target.validate().is_ok() =>
        {
            Stage::Preparation(target)
        }
        (writer_preparation::VERIFIER_OPERATION, Some(target @ Target::VerifierModules { .. }), None, None, None)
            if target.validate().is_ok() =>
        {
            Stage::Preparation(target)
        }
        (
            writer_preparation::GROUPED_VERIFIER_OPERATION,
            Some(target @ Target::VerifierGroups { .. }),
            None,
            None,
            None,
        ) if target.validate().is_ok() => Stage::Preparation(target),
        (REGROUP, Some(target @ Target::Regroup(_)), None, None, None) if target.validate().is_ok() => {
            Stage::Preparation(target)
        }
        (FIRST_PARTY_CLEAN, Some(target @ Target::FirstPartyClean { .. }), None, None, None) => {
            Stage::Preparation(target)
        }
        (TOOLING_SUITES, Some(target @ Target::ToolingSuites { .. }), None, None, None) => Stage::Preparation(target),
        (WHOLE_LINT, Some(target @ Target::WholeLint { .. }), None, None, None) => Stage::Preparation(target),
        (CONSUMER, None, Some(source), None, None) if source.valid() => Stage::Consumer(source),
        (FORMAT_FIX, None, None, Some(formatter), None) if formatter.valid() => Stage::Formatter(formatter),
        (FORMAT_PREVIEW, None, None, Some(formatter), None) if formatter.valid_preview() => {
            Stage::FormatterPreview(formatter)
        }
        (operation, None, None, None, Some(request)) if request.valid() => {
            let projection = setup::Projection::from_operation(operation)
                .filter(|projection| request.matches_projection(*projection))?;
            Stage::SetupVerification(request, projection)
        }
        _ => return None,
    };
    if request.job.is_empty() || request.revision == 0 {
        return None;
    }
    Some((request.project, stage))
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedReadiness {
    pub(crate) readiness: Receipt,
    pub(crate) prepared_generation: String,
}

pub(super) enum InspectionReadiness {
    Ready,
    Prepared(String),
}

pub(super) fn prepared_inspection_receipt(
    snapshot: &Snapshot,
    project: &Path,
    imports: &[String],
    job: &[u8],
    revision: u64,
) -> Option<InspectionReadiness> {
    use crate::native_store::store::ResultKnowledge;

    let original = serde_json::from_slice::<Intent>(&snapshot.intent).ok()?;
    if original.job != job || original.revision != revision {
        return None;
    }
    let (writer_project, Stage::Preparation(target @ Target::Ready { package: None, .. })) =
        parse_intent(&snapshot.intent)?
    else {
        return None;
    };
    let Target::Ready { name, .. } = &target else {
        return None;
    };
    if writer_project != project || !imports.contains(name) {
        return None;
    }
    let (ResultKnowledge::Retained { body, .. } | ResultKnowledge::Acknowledged { body, .. }) = &snapshot.result else {
        return None;
    };
    if let Ok(bound) = serde_json::from_slice::<PreparedReadiness>(body) {
        let receipt = Receipt::parse(&serde_json::to_vec(&bound.readiness).ok()?, &target).ok()?;
        return receipt
            .target_ready()
            .then_some(InspectionReadiness::Prepared(bound.prepared_generation));
    }
    Receipt::parse(body, &target)
        .ok()?
        .target_ready()
        .then_some(InspectionReadiness::Ready)
}

// Opt-in, operator-owned output of the existing guarded generation producer.
// The readiness writer validates it under the same exclusive artifact union
// before putting its bytes into the sealed stage receipt.
fn bind_ready_record(
    receipt: Receipt,
    target: &Target,
    root: &Path,
    domains: &[PathBuf],
) -> io::Result<serde_json::Value> {
    use lean_host_mcp_worker::prepared_project::PreparedGeneration;
    use std::{io::Read as _, os::unix::fs::MetadataExt as _};

    if let Target::Ready { name, package: None } = target
        && receipt.target_ready()
        && let Some(path) = std::env::var_os("LEAN_HOST_MCP_MANAGED_READY_RECORD").map(PathBuf::from)
    {
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.nlink() != 1
            || metadata.mode() & 0o022 != 0
            || path.canonicalize()? != path
        {
            return Err(io::Error::other(
                "managed preparation record is not a canonical operator-owned file",
            ));
        }
        let file = std::fs::File::open(&path)?;
        let opened = file.metadata()?;
        if (opened.dev(), opened.ino()) != (metadata.dev(), metadata.ino()) {
            return Err(io::Error::other("managed preparation record changed while opening"));
        }
        let mut record = String::new();
        file.take(4098).read_to_string(&mut record)?;
        if record.len() > 4097 {
            return Err(io::Error::other("managed preparation record exceeds its input bound"));
        }
        let record = record.trim().to_owned();
        let approved = PreparedGeneration::decode(&record)?;
        if approved.project() != root
            || !approved
                .modules()
                .iter()
                .any(|(module, owner)| module == name && owner == root)
        {
            return Err(io::Error::other(
                "managed preparation record does not bind the selected root module",
            ));
        }
        PreparedGeneration::check_identity(&record, domains)?;
        approved.check_products()?;
        return Ok(serde_json::to_value(PreparedReadiness {
            readiness: receipt,
            prepared_generation: record,
        })?);
    }
    Ok(serde_json::to_value(receipt)?)
}

pub(crate) fn validate_create(intent: &[u8]) -> Result<(), crate::native_store::store::Reject> {
    let needs_validation = serde_json::from_slice::<serde_json::Value>(intent).is_ok_and(|value| {
        matches!(
            value.get("operation").and_then(serde_json::Value::as_str),
            Some(
                writer_preparation::OPERATION
                    | writer_preparation::GROUPED_VERIFIER_OPERATION
                    | REGROUP
                    | SETUP_TRIAL
                    | SETUP_PROOF_POSITION
                    | SETUP_PROOF_STEP
                    | SETUP_PROOF_SEARCH
            )
        )
    });
    if needs_validation && parse_intent(intent).is_none() {
        return Err(crate::native_store::store::Reject::InvalidReadinessIntent);
    }
    Ok(())
}

pub(super) async fn run(
    broker: &ProjectBroker,
    store: &Arc<Mutex<NativeStore>>,
    original: Snapshot,
    capability: Capability,
) -> Result<(), McpError> {
    let invalid = |reason| McpError::invalid_params(reason, None);
    let (project, stage) = parse_intent(&original.intent).ok_or_else(|| invalid("invalid_readiness_intent"))?;
    let root = broker
        .native_root(&ProjectHint::Default)
        .map_err(|_| invalid("readiness_project_unavailable"))?;
    if root.as_os_str() != project.as_os_str() {
        return Err(invalid("readiness_project_changed"));
    }
    let helper = selection().map_err(|_| invalid("operation_disabled"))?;
    if matches!(&stage, Stage::Preparation(Target::Ready { package: None, .. }))
        && std::env::var_os(writer_preparation::PLAN_ENV).is_some()
    {
        // Validate the successor's IPC path before consuming the preparation budget or opening a writer.
        crate::native_socket::directory().map_err(|error| McpError::invalid_params(error.to_string(), None))?;
    }
    let deadline = original
        .admission_deadline
        .ok_or_else(|| invalid("missing_admission_deadline"))?;
    let slot = broker
        .try_managed_stage_slot(&root)
        .map_err(|_| McpError::internal_error("readiness_busy", None))?;
    let store = Arc::clone(store);
    let prepared_generation = broker.prepared_generation().map(str::to_owned);
    if let Stage::SetupVerification(request, _) = &stage
        && !request.matches_prepared(prepared_generation.as_deref())
    {
        return Err(invalid("setup_verification_prepared_generation_changed"));
    }
    tokio::task::spawn_blocking(move || {
        let mut slot = crate::worker_retirement::ManagedWriterSlot::new(slot);
        execute(
            &helper,
            &root,
            &store,
            &original,
            &capability,
            deadline,
            &stage,
            prepared_generation,
            &mut slot,
        )
    })
    .await
    .map_err(|_| McpError::internal_error("readiness_writer_unavailable", None))?
}

fn execute(
    helper: &Path,
    root: &Path,
    store: &Mutex<NativeStore>,
    original: &Snapshot,
    capability: &Capability,
    deadline: AdmissionDeadline,
    stage: &Stage,
    prepared_generation: Option<String>,
    slot: &mut crate::worker_retirement::ManagedWriterSlot,
) -> Result<(), McpError> {
    let no_start = |_| McpError::invalid_params("readiness_writer_not_started", None);
    let ownership_registry = crate::shared_writer::ownership_registry().map_err(no_start)?;
    let invalid_lint_input = || McpError::invalid_params("whole_library_lint_configuration_changed", None);
    if let Stage::Preparation(Target::WholeLint { lakefile_sha256 }) = stage {
        writer_preparation::whole_lint::check_config(root, lakefile_sha256).map_err(|_| invalid_lint_input())?;
    }
    if let Stage::Preparation(Target::FirstPartyClean { lakefile_sha256 }) = stage {
        let unsafe_clean = |_| McpError::invalid_params("first_party_clean_input_or_scope_changed", None);
        let build = writer_preparation::first_party_clean::check(root, lakefile_sha256).map_err(unsafe_clean)?;
        if helper.starts_with(&build)
            || std::env::current_exe().map_err(unsafe_clean)?.starts_with(&build)
            || store
                .lock()
                .database_path()
                .is_some_and(|path| path.starts_with(&build))
            || ownership_registry.starts_with(&build)
            || std::env::temp_dir().starts_with(&build)
        {
            return Err(McpError::invalid_params(
                "first_party_clean_overlaps_native_state",
                None,
            ));
        }
    }
    let expired = || deadline.remaining().is_err();
    let shims = if matches!(stage, Stage::SetupVerification(..)) {
        let meta = crate::LakeProjectMeta::from_explicit(root)
            .map_err(|_| McpError::invalid_params("setup_verification_not_started", None))?;
        Some(
            crate::broker::verify_file::VerifierShims::select(&meta)
                .map_err(|_| McpError::invalid_params("setup_verification_not_started", None))?,
        )
    } else {
        None
    };
    let tooling_root = if matches!(
        stage,
        Stage::Preparation(Target::ToolingSuites { .. } | Target::Regroup(_))
    ) {
        Some(writer_preparation::tooling_suites::package_root(root).map_err(no_start)?)
    } else {
        None
    };
    let runtime = if let Stage::SetupVerification(request, setup::Projection::ProofSearch) = stage {
        let proposed = request
            .proof_search
            .as_ref()
            .ok_or_else(|| no_start(io::Error::other("missing proof search")))?;
        let runtime = crate::tools::proof_search::RuntimeSelection::configured()
            .map_err(|_| McpError::invalid_params("semantic_runtime_unavailable", None))?;
        if runtime.sha256 != proposed.runtime_sha256 {
            return Err(McpError::invalid_params("semantic_runtime_changed", None));
        }
        Some(runtime)
    } else {
        None
    };
    let mut roots = shims
        .as_ref()
        .map_or(tooling_root.as_slice(), |shims| shims.domains.as_slice())
        .to_vec();
    if let Some(runtime) = &runtime {
        roots.push(
            runtime
                .package()
                .map_err(|_| McpError::invalid_params("semantic_runtime_unavailable", None))?
                .to_owned(),
        );
    }
    let budget = deadline.remaining().map_err(|error| super::public_error(&error))?;
    let mut writer = SharedWriter::open_with_write_roots_handoff_observed(
        helper,
        root,
        &roots,
        budget,
        &expired,
        slot.observation(),
    )
    .map_err(no_start)?;
    if let Stage::Preparation(Target::ToolingSuites {
        lakefile_sha256,
        manifest_sha256,
    }) = stage
        && writer_preparation::tooling_suites::check(root, lakefile_sha256, manifest_sha256).is_err()
    {
        let binding = writer.stage_ready().map_err(no_start).and_then(|ready| {
            store
                .lock()
                .bind_readiness_writer(
                    original.key,
                    capability,
                    &original.intent,
                    ready,
                    deadline,
                    ReadinessDecision::RejectToolingInputs,
                )
                .map_err(|error| super::public_error(&error))
        });
        writer
            .close(Duration::from_secs(5))
            .map_err(|_| McpError::internal_error("readiness_custody_unresolved", None))?;
        if let Some(binding) = binding? {
            store
                .lock()
                .finish_tooling_input_refusal(binding, capability)
                .map_err(|error| super::public_error(&error))?;
        }
        return Ok(());
    }
    let cancellation = LeanWorkerCancellationToken::default();
    let prepared = (|| {
        let ready = writer.stage_ready().map_err(no_start)?.clone();
        let command = match stage {
            Stage::Preparation(target) => {
                let plan = Plan {
                    root: root.to_owned(),
                    target: target.clone(),
                    boot: deadline.boot,
                    expires_ns: deadline.expires_ns,
                };
                let mut command = Command::new(
                    std::env::current_exe()
                        .map_err(no_start)?
                        .canonicalize()
                        .map_err(no_start)?,
                );
                command
                    .arg("writer-prepare")
                    .arg(plan.encode().map_err(no_start)?)
                    .current_dir(root)
                    .env("LAKE_CACHE_DIR", writer.asset_dir())
                    .env("TMPDIR", writer.asset_dir());
                if let Some(policy) = std::env::var_os(writer_preparation::PLAN_ENV) {
                    command.env(writer_preparation::PLAN_ENV, policy);
                }
                command
            }
            Stage::SetupVerification(request, projection) => {
                let source_changed = match projection {
                    setup::Projection::CommandTrial => Some("managed_trial_source_changed"),
                    setup::Projection::ProofStep => Some("managed_proof_step_source_changed"),
                    setup::Projection::ProofSearch => Some("managed_proof_search_source_changed"),
                    setup::Projection::ProofPosition => Some("managed_proof_position_source_changed"),
                    setup::Projection::Verify | setup::Projection::Diagnostics | setup::Projection::Outline => None,
                };
                if let Some(reason) = source_changed {
                    setup::read_source(root, request).map_err(|_| McpError::invalid_params(reason, None))?;
                }
                let shims = shims
                    .as_ref()
                    .ok_or_else(|| McpError::invalid_params("setup_verification_not_started", None))?;
                let plan = setup::Plan {
                    root: root.to_owned(),
                    worker: shims.worker.clone(),
                    domains: writer.write_roots().to_vec(),
                    prepared: prepared_generation,
                    request: request.clone(),
                    projection: *projection,
                    boot: deadline.boot,
                    expires_ns: deadline.expires_ns,
                    semantic_runtime: runtime,
                };
                let mut command = Command::new(
                    std::env::current_exe()
                        .map_err(no_start)?
                        .canonicalize()
                        .map_err(no_start)?,
                );
                command
                    .arg("writer-verify-setup")
                    .arg(
                        serde_json::to_string(&plan)
                            .map_err(|_| McpError::invalid_params("setup_verification_not_started", None))?,
                    )
                    .current_dir(root)
                    .env("TMPDIR", writer.asset_dir());
                command
            }
            Stage::Cache => lake_command(root, writer.asset_dir()).map_err(no_start)?,
            Stage::Consumer(source) => {
                source
                    .check(root)
                    .map_err(|_| McpError::invalid_params("configured_consumer_source_changed", None))?;
                source.command(root, writer.asset_dir()).map_err(no_start)?
            }
            Stage::Formatter(request) | Stage::FormatterPreview(request) => {
                request
                    .check_sources(root)
                    .map_err(|_| McpError::invalid_params("formatter_source_changed", None))?;
                request
                    .command(root, writer.asset_dir())
                    .map_err(|_| McpError::invalid_params("approved_formatter_unavailable", None))?
            }
        };
        let permit = WriterStagePermit::new(
            ready.clone(),
            &uuid::Uuid::from_bytes(deadline.boot).to_string(),
            deadline.expires_ns,
        )
        .map_err(no_start)?;
        let mut store = store.lock();
        let binding = store
            .bind_readiness_writer(
                original.key,
                capability,
                &original.intent,
                &ready,
                deadline,
                ReadinessDecision::Launch,
            )
            .map_err(|error| super::public_error(&error))?;
        if let Some(binding) = binding {
            store
                .attach_active_stop(binding, cancellation.clone())
                .map_err(|error| super::public_error(&error))?;
        }
        drop(store);
        Ok::<_, McpError>(binding.map(|binding| (binding, command, permit)))
    })();
    let (binding, command, permit) = match prepared {
        Ok(Some(stage)) => stage,
        other => {
            writer
                .close(Duration::from_secs(5))
                .map_err(|_| McpError::internal_error("readiness_custody_unresolved", None))?;
            return other.map(|_| ());
        }
    };
    let _stop = super::StopRegistration { store, binding };

    if option_env!("LEAN_HOST_MCP_COMPILE_DISPOSABLE_CUT").is_some()
        && std::env::var_os("LEAN_HOST_MCP_TEST_READINESS_READY_STOP").is_some_and(|path| Path::new(&path).is_file())
    {
        let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::STOP);
    }

    // A partial or failed send cannot certify that the helper saw no command.
    slot.begin_dispatch();
    let result = writer.run_permitted(&command, permit, &|| cancellation.is_cancelled() || expired());
    // Preserve the primary waited result independently of helper settlement.
    // The caller waits for closed custody before using it.
    let body = writer_body(result, |output| match stage {
        Stage::Preparation(Target::VerifierGroups { groups }) if output.status.success() => {
            writer_preparation::GroupedVerifierReceipt::parse(&output.stdout, groups.len())
                .ok()
                .and_then(|receipt| serde_json::to_value(receipt).ok())
        }
        Stage::Preparation(Target::FirstPartyClean { .. }) if output.status.success() => {
            writer_preparation::first_party_clean::Receipt::parse(&output.stdout)
                .ok()
                .and_then(|receipt| serde_json::to_value(receipt).ok())
        }
        Stage::Preparation(Target::Regroup(_)) if output.status.success() => {
            writer_preparation::regroup::Receipt::parse(&output.stdout)
                .ok()
                .and_then(|receipt| serde_json::to_value(receipt).ok())
        }
        Stage::Preparation(Target::ToolingSuites { .. }) if output.status.success() => {
            writer_preparation::tooling_suites::Receipt::parse(&output.stdout)
                .ok()
                .and_then(|receipt| serde_json::to_value(receipt).ok())
        }
        Stage::Preparation(Target::WholeLint { .. }) if output.status.success() => {
            writer_preparation::whole_lint::Receipt::parse(&output.stdout)
                .ok()
                .and_then(|receipt| serde_json::to_value(receipt).ok())
        }
        Stage::Preparation(Target::RootLibrary {
            name,
            module_count,
            blueprint,
            ..
        }) if output.status.success() => {
            writer_preparation::root_library::Receipt::parse(&output.stdout, name, *module_count, blueprint.is_some())
                .ok()
                .and_then(|receipt| serde_json::to_value(receipt).ok())
        }
        Stage::Preparation(target @ Target::Ready { package: None, .. })
            if output.status.success() && std::env::var_os(writer_preparation::PLAN_ENV).is_some() =>
        {
            serde_json::from_slice::<PreparedReadiness>(&output.stdout)
                .ok()
                .and_then(|bound| {
                    let bytes = serde_json::to_vec(&bound.readiness).ok()?;
                    let receipt = Receipt::parse(&bytes, target).ok()?;
                    let record =
                        lean_host_mcp_worker::prepared_project::PreparedGeneration::decode(&bound.prepared_generation)
                            .ok()?;
                    (receipt.target_ready() && record.is_derived() && record.project() == root)
                        .then(|| serde_json::to_value(bound).ok())
                        .flatten()
                })
        }
        Stage::Preparation(target) if output.status.success() => Receipt::parse(&output.stdout, target)
            .and_then(|receipt| bind_ready_record(receipt, target, root, writer.write_roots()))
            .map_err(|error| tracing::debug!(%error, "managed preparation result unavailable"))
            .ok(),
        Stage::Preparation(_) => None,
        Stage::SetupVerification(request, projection) => request.receipt(*projection, &output),
        Stage::Cache => output
            .status
            .code()
            .map(|exit_code| serde_json::json!({"version":1,"stage":"mathlib-cache-check","exit_code":exit_code})),
        Stage::Consumer(source) => Some(source.receipt(root, output)),
        Stage::Formatter(request) => Some(request.receipt(root, writer.asset_dir(), &output)),
        Stage::FormatterPreview(request) => Some(request.preview_receipt(writer.asset_dir(), &output)),
    });
    let saved = store
        .lock()
        .observe_result(
            binding,
            capability,
            uuid::Uuid::new_v4().into_bytes(),
            body.to_string().as_bytes(),
        )
        .map_err(|error| super::public_error(&error));
    if writer.close(Duration::from_secs(5)).is_ok() {
        store
            .lock()
            .observe_resource(binding, capability, ResourceReport::Closed)
            .map_err(|error| super::public_error(&error))?;
    }
    saved
}

#[cfg(test)]
#[test]
fn unavailable_writer_result_survives_get_without_changing_execution_or_custody()
-> Result<(), Box<dyn std::error::Error>> {
    use crate::native_store::store::{Consume, NativeKey, Scope};
    use serde_json::json;
    use std::process::ExitStatus;

    let root = tempfile::tempdir()?;
    let path = root.path().canonicalize()?.join("stage.sqlite3");
    let mut store = NativeStore::create_new(&path)?;
    let capability = Capability([2; 32]);
    let output = |status| Output {
        status: ExitStatus::from_raw(status),
        stdout: b"invalid receipt".to_vec(),
        stderr: b"private diagnostic".to_vec(),
    };
    let error = io::Error::new(
        io::ErrorKind::TimedOut,
        WriterFailure {
            disposition: WriterDisposition::Unknown,
            detail: "private diagnostic".to_owned(),
            stop_receipt: None,
        },
    );
    let cases = [
        (Err(error), "writer_error"),
        (Ok(output(7 << 8)), "writer_command_failed"),
        (Ok(output(0)), "receipt_rejected"),
    ];
    for ((outcome, reason), nonce) in cases.into_iter().zip(10..13) {
        let key = NativeKey {
            store: store.identity(),
            epoch: 1,
            nonce: [nonce; 16],
            stage: 1,
        };
        let create = json!({"version": 1, "command": {
            "op": "create", "key": key, "capability": capability.0, "intent": b"writer diagnostic",
        }});
        store.admission(create.to_string().as_bytes())?;
        let binding = store.bind_scope(
            key,
            &capability,
            Scope {
                id: [nonce; 16],
                generation: 1,
                owner: [3; 16],
            },
        )?;
        assert!(matches!(
            store.consume_launch(binding, &capability)?,
            Consume::Granted(_)
        ));
        store.observe_resource(binding, &capability, ResourceReport::Open)?;

        let body = writer_body(outcome, |_| None);
        assert_eq!(body["reason"], reason);
        assert_eq!(body["status"], "unavailable");
        assert!(!body.to_string().contains("private diagnostic"));
        assert!(body.get("build_exit_code").is_none());
        if reason == "writer_error" {
            assert_eq!(body["writer_disposition"], "unknown");
        } else if reason == "writer_command_failed" {
            assert_eq!(body["command_exit_code"], 7);
        }
        store.observe_result(binding, &capability, [4; 16], body.to_string().as_bytes())?;
        drop(store);
        store = NativeStore::open(&path)?;

        let get = json!({"version": 1, "command": {"op": "get", "key": key, "capability": capability.0}});
        let reply = store.admission(get.to_string().as_bytes())?;
        assert_eq!(reply["execution"], "unknown");
        assert_eq!(reply["custody"], "open");
        let retained: Vec<u8> = serde_json::from_value(reply["result"]["body"].clone())?;
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&retained)?, body);
        assert!(matches!(
            store.consume_launch(binding, &capability)?,
            Consume::AlreadyConsumed
        ));

        store.observe_resource(binding, &capability, ResourceReport::Closed)?;
        let closed = store.admission(get.to_string().as_bytes())?;
        assert_eq!(closed["custody"], "closed");
        assert_eq!(closed["execution"], "unknown");
        assert_eq!(closed["result"], reply["result"]);
    }

    let valid_verdict = json!({"exit_code": 1});
    assert_eq!(
        writer_body(Ok(output(1 << 8)), |_| Some(valid_verdict.clone())),
        valid_verdict
    );
    Ok(())
}
