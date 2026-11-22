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
    native_store::store::{AdmissionDeadline, Capability, NativeStore, ResourceReport, Snapshot},
    writer_preparation::{self, Plan, Receipt, Target},
};

#[path = "configured_consumer.rs"]
mod configured_consumer;
pub(super) const CONSUMER: &str = "lake_configured_consumer_v1";
use configured_consumer::SourceIdentity;
#[path = "formatter_fix.rs"]
mod formatter_fix;
pub(super) const FORMAT_FIX: &str = "lean_formatter_fix_v1";

pub(super) fn formatter_digest(root: &Path) -> Option<[u8; 32]> {
    formatter_fix::Approval::select(root).ok().map(|pin| pin.sha256)
}

enum Stage {
    Cache,
    Preparation(Target),
    Consumer(SourceIdentity),
    Formatter(formatter_fix::Request),
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
    ) {
        (OPERATION, None, None, None) => Stage::Cache,
        (writer_preparation::OPERATION, Some(target), None, None)
            if !matches!(target, Target::VerifierModules { .. }) && target.validate().is_ok() =>
        {
            Stage::Preparation(target)
        }
        (writer_preparation::VERIFIER_OPERATION, Some(target @ Target::VerifierModules { .. }), None, None)
            if target.validate().is_ok() =>
        {
            Stage::Preparation(target)
        }
        (CONSUMER, None, Some(source), None) if source.valid() => Stage::Consumer(source),
        (FORMAT_FIX, None, None, Some(formatter)) if formatter.valid() => Stage::Formatter(formatter),
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
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        execute(&helper, &root, &store, original, &capability, deadline, stage)
    })
    .await
    .map_err(|_| McpError::internal_error("readiness_writer_unavailable", None))?
}

fn execute(
    helper: &Path,
    root: &Path,
    store: &Mutex<NativeStore>,
    original: Snapshot,
    capability: &Capability,
    deadline: AdmissionDeadline,
    stage: Stage,
) -> Result<(), McpError> {
    let no_start = |_| McpError::invalid_params("readiness_writer_not_started", None);
    let budget = deadline.remaining().map_err(super::public_error)?;
    let expired = || deadline.remaining().is_err();
    let mut writer = SharedWriter::open_cancellable(helper, root, budget, &expired).map_err(no_start)?;
    let cancellation = LeanWorkerCancellationToken::default();
    let prepared = (|| {
        let ready = writer.stage_ready().map_err(no_start)?.clone();
        let command = match &stage {
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
            Stage::Cache => lake_command(root, writer.asset_dir()).map_err(no_start)?,
            Stage::Consumer(source) => {
                source
                    .check(root)
                    .map_err(|_| McpError::invalid_params("configured_consumer_source_changed", None))?;
                source.command(root, writer.asset_dir()).map_err(no_start)?
            }
            Stage::Formatter(request) => {
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
            .consume_readiness_writer(original.key, capability, &original.intent, &ready, deadline)
            .map_err(super::public_error)?;
        if let Some(binding) = binding {
            store
                .attach_active_stop(binding, cancellation.clone())
                .map_err(super::public_error)?;
        }
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
    let body = result.ok().and_then(|output| match &stage {
        Stage::Preparation(target) if output.status.success() => Receipt::parse(&output.stdout, target)
            .ok()
            .and_then(|receipt| serde_json::to_value(receipt).ok()),
        Stage::Preparation(_) => None,
        Stage::Cache => output
            .status
            .code()
            .map(|exit_code| serde_json::json!({"version":1,"stage":"mathlib-cache-check","exit_code":exit_code})),
        Stage::Consumer(source) => Some(source.receipt(root, output)),
        Stage::Formatter(request) => Some(request.receipt(root, writer.asset_dir(), &output)),
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
            .map_err(super::public_error)
    });
    if writer.close(Duration::from_secs(5)).is_ok() {
        store
            .lock()
            .observe_resource(binding, capability, ResourceReport::Closed)
            .map_err(super::public_error)?;
    }
    saved
}
