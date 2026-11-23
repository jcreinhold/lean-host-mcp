//! One fixed Lake cache-readiness stage with native writer custody.
use std::{
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use lean_rs_worker_parent::LeanWorkerCancellationToken;
use lean_toolchain::shared_writer::{SharedWriter, WriterStagePermit};
use parking_lot::Mutex;
use rmcp::ErrorData as McpError;
use serde::Deserialize;
use tokio::sync::Semaphore;

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
pub(super) use setup::{DIAGNOSTICS as SETUP_DIAGNOSTICS, OPERATION as SETUP_VERIFICATION, OUTLINE as SETUP_OUTLINE};
pub(super) use writer_preparation::first_party_clean::OPERATION as FIRST_PARTY_CLEAN;
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

pub(super) const OPERATION: &str = "lake_mathlib_cache_readiness_v1";
static SLOT: Semaphore = Semaphore::const_new(1);

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
                    | CONSUMER
                    | FORMAT_FIX
                    | FORMAT_PREVIEW
                    | SETUP_VERIFICATION
                    | SETUP_DIAGNOSTICS
                    | SETUP_OUTLINE
                    | TOOLING_SUITES
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

pub(super) async fn run(
    broker: &ProjectBroker,
    store: &Arc<Mutex<NativeStore>>,
    original: Snapshot,
    capability: Capability,
) -> Result<(), McpError> {
    let invalid = |reason| McpError::invalid_params(reason, None);
    let request: Intent = serde_json::from_slice(&original.intent).map_err(|_| invalid("invalid_readiness_intent"))?;
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
                .filter(|projection| request.matches_projection(*projection))
                .ok_or_else(|| invalid("invalid_readiness_intent"))?;
            Stage::SetupVerification(request, projection)
        }
        _ => return Err(invalid("invalid_readiness_intent")),
    };
    if request.job.is_empty() || request.revision == 0 {
        return Err(invalid("invalid_readiness_intent"));
    }
    let root = broker
        .native_root(&ProjectHint::Default)
        .map_err(|_| invalid("readiness_project_unavailable"))?;
    if root.as_os_str() != request.project.as_os_str() {
        return Err(invalid("readiness_project_changed"));
    }
    let helper = selection().map_err(|_| invalid("operation_disabled"))?;
    let deadline = original
        .admission_deadline
        .ok_or_else(|| invalid("missing_admission_deadline"))?;
    let slot = SLOT
        .try_acquire()
        .map_err(|_| McpError::internal_error("readiness_busy", None))?;
    let store = Arc::clone(store);
    let prepared_generation = broker.prepared_generation().map(str::to_owned);
    if let Stage::SetupVerification(request, _) = &stage
        && !request.matches_prepared(prepared_generation.as_deref())
    {
        return Err(invalid("setup_verification_prepared_generation_changed"));
    }
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        execute(
            &helper,
            &root,
            &store,
            &original,
            &capability,
            deadline,
            &stage,
            prepared_generation,
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
) -> Result<(), McpError> {
    let no_start = |_| McpError::invalid_params("readiness_writer_not_started", None);
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
            || lean_toolchain::native_guardian::registry_directory()
                .map_err(unsafe_clean)?
                .starts_with(&build)
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
    let roots = shims.as_ref().map_or(&[][..], |shims| shims.domains.as_slice());
    let budget = deadline.remaining().map_err(|error| super::public_error(&error))?;
    let mut writer = SharedWriter::open_with_write_roots(helper, root, roots, budget, &expired).map_err(no_start)?;
    if let Stage::Preparation(Target::ToolingSuites { executable_sha256 }) = stage
        && writer_preparation::tooling_suites::check(root, executable_sha256, &|| {
            deadline.remaining().map_err(io::Error::other)
        })
        .is_err()
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
                    ReadinessDecision::RejectPrebuilt,
                )
                .map_err(|error| super::public_error(&error))
        });
        writer
            .close(Duration::from_secs(5))
            .map_err(|_| McpError::internal_error("readiness_custody_unresolved", None))?;
        if let Some(binding) = binding? {
            store
                .lock()
                .finish_prebuilt_refusal(binding, capability)
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
                command
            }
            Stage::SetupVerification(request, projection) => {
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

    let result = writer.run_permitted(&command, permit, &|| cancellation.is_cancelled() || expired());
    // Preserve the primary waited result independently of helper settlement.
    // The caller waits for closed custody before using it.
    let body = result.ok().and_then(|output| match stage {
        Stage::Preparation(Target::FirstPartyClean { .. }) if output.status.success() => {
            writer_preparation::first_party_clean::Receipt::parse(&output.stdout)
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
        Stage::Preparation(target) if output.status.success() => Receipt::parse(&output.stdout, target)
            .ok()
            .and_then(|receipt| serde_json::to_value(receipt).ok()),
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
    let saved = body.map_or(Ok(()), |body| {
        store
            .lock()
            .observe_result(
                binding,
                capability,
                uuid::Uuid::new_v4().into_bytes(),
                body.to_string().as_bytes(),
            )
            .map_err(|error| super::public_error(&error))
    });
    if writer.close(Duration::from_secs(5)).is_ok() {
        store
            .lock()
            .observe_resource(binding, capability, ResourceReport::Closed)
            .map_err(|error| super::public_error(&error))?;
    }
    saved
}
