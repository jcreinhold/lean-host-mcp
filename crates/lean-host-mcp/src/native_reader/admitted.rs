//! Durable launch gate for one fixed external reader.
use super::{
    Capture, NativeReaderArgs, NativeReaderProgram, NativeReaderResult, POLL, invalid, reserve_scope, run_internal,
    until,
};
use lean_rs_worker_parent::LeanWorkerNativeScopeLease;
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use serde::{Deserialize, Serialize};
use std::{io::Write as _, os::unix::ffi::OsStrExt as _, sync::Arc};
use std::{
    io::{self, Read as _},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use lean_rs_worker_parent::LeanWorkerCancellationToken;
use parking_lot::Mutex;
use rmcp::ErrorData as McpError;

use crate::{
    ProjectBroker,
    broker::ProjectHint,
    native_store::store::{
        AdmissionDeadline, Binding, CallerKey, Capability, Consume, Gate, NativeKey, NativeStore, ResourceReport,
        SavedRequest, Scope, recovery::RegistryIdentity,
    },
    toolchain::{ToolchainId, WindowVerdict},
};

#[path = "module_lint.rs"]
mod module_lint;
pub(crate) use module_lint::{CHECKPOINT as CHECKPOINT_LINT, OPERATION as MODULE_LINT};

pub(crate) const OPERATION: &str = "lean_import_scan_v1";
pub(crate) const AXIOMS: &str = "lean_declaration_axioms_v1";
const MAX_NAMES: usize = 64;
const MAX_NAME_BYTES: usize = 256;
pub(super) const DEADLINE_ENV: &str = "LEAN_HOST_MCP_ADMITTED_EXTERNAL_READER_DEADLINE";
const READY: u8 = b'R';
const GO: u8 = b'G';

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Deadline {
    boot: [u8; 16],
    expires_ns: u64,
}

impl Deadline {
    pub(super) fn remaining(self) -> io::Result<Duration> {
        AdmissionDeadline {
            boot: self.boot,
            expires_ns: self.expires_ns,
        }
        .remaining()
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "original external reader deadline unavailable or elapsed",
            )
        })
    }
}

pub(super) fn bootstrap_deadline() -> io::Result<Option<(String, Deadline)>> {
    match std::env::var(DEADLINE_ENV) {
        Ok(encoded) if encoded.len() <= 512 => {
            let deadline = serde_json::from_str(&encoded)?;
            Ok(Some((encoded, deadline)))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        _ => Err(invalid("invalid admitted external reader deadline")),
    }
}

pub(super) fn await_go(deadline: Deadline) -> io::Result<()> {
    deadline.remaining()?;
    let mut output = io::stdout().lock();
    output.write_all(&[READY])?;
    output.flush()?;
    drop(output);

    let input = io::stdin();
    fcntl_setfl(&input, fcntl_getfl(&input)? | OFlags::NONBLOCK)?;
    let mut input = input.lock();
    let mut byte = [0];
    loop {
        let budget = deadline.remaining()?;
        match input.read(&mut byte) {
            Ok(1) if byte == [GO] => return verify_go(deadline),
            Ok(_) => return Err(invalid("original external reader GO missing")),
            Err(error) if matches!(error.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {
                thread::sleep(POLL.min(budget));
            }
            Err(error) => return Err(error),
        }
    }
}

fn verify_go(deadline: Deadline) -> io::Result<()> {
    // The byte could have been buffered while the stage was still valid.
    deadline.remaining().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn axiom_input_and_reports_preserve_order_and_require_exact_coverage() {
        let imports = ["Z.First".to_owned(), "A.Second".to_owned()];
        let declarations = ["Demo.closed".to_owned(), "Demo.choice".to_owned()];
        assert_eq!(
            axiom_input(&imports, &declarations).unwrap(),
            b"import Z.First\nimport A.Second\n#print axioms Demo.closed\n#print axioms Demo.choice\n"
        );
        assert!(axiom_input(&imports, &["Demo.x\n#eval 1".to_owned()]).is_err());
        let closed = r#"{"data":"'Demo.closed' does not depend on any axioms","severity":"information"}"#;
        let choice =
            r#"{"data":"'Demo.choice' depends on axioms: [Classical.choice, propext]","severity":"information"}"#;
        assert_eq!(
            axiom_reports(&format!("{closed}\n{choice}\n"), &declarations).unwrap(),
            [vec![], vec!["Classical.choice".to_owned(), "propext".to_owned()]]
        );
        assert!(axiom_reports(closed, &declarations).is_err());
        assert!(axiom_reports(&format!("{choice}\n{closed}"), &declarations).is_err());
        assert!(axiom_reports(&format!("{closed}\n{choice}\n{choice}"), &declarations).is_err());
    }

    #[test]
    fn buffered_go_is_rejected_on_the_original_boot_clock_after_expiry() {
        let admitted = crate::native_store::store::recovery::admission_deadline(Duration::from_millis(100)).unwrap();
        let deadline = Deadline {
            boot: admitted.boot,
            expires_ns: admitted.expires_ns,
        };
        let (mut sender, mut receiver) = UnixStream::pair().unwrap();
        sender.write_all(&[GO]).unwrap();
        let mut buffered = [0];
        receiver.read_exact(&mut buffered).unwrap();
        assert_eq!(buffered, [GO]);
        assert!(verify_go(deadline).is_ok());
        while deadline.remaining().is_ok() {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(verify_go(deadline).unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    operation: String,
    job: Vec<u8>,
    revision: u64,
    project: PathBuf,
    file: Option<PathBuf>,
    imports: Option<Vec<String>>,
    declarations: Option<Vec<String>>,
    modules: Option<Vec<String>>,
    checkpoint: Option<module_lint::Checkpoint>,
}

fn qualified_name(name: &str) -> bool {
    name.len() <= MAX_NAME_BYTES
        && name.split('.').all(|part| {
            let mut chars = part.chars();
            chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
                && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '\''))
        })
}

fn axiom_input(imports: &[String], declarations: &[String]) -> io::Result<Vec<u8>> {
    use std::fmt::Write as _;
    if [imports, declarations]
        .iter()
        .any(|names| names.is_empty() || names.len() > MAX_NAMES || !names.iter().all(|name| qualified_name(name)))
    {
        return Err(invalid("external axiom reader requires bounded qualified names"));
    }
    let mut source = String::new();
    for module in imports {
        writeln!(source, "import {module}").map_err(io::Error::other)?;
    }
    for declaration in declarations {
        writeln!(source, "#print axioms {declaration}").map_err(io::Error::other)?;
    }
    Ok(source.into_bytes())
}

fn axiom_reports(output: &str, declarations: &[String]) -> io::Result<Vec<Vec<String>>> {
    let mut lines = output.lines();
    let reports = declarations
        .iter()
        .map(|name| {
            let line = lines.next().ok_or_else(|| invalid("missing axiom report"))?;
            let message: serde_json::Value = serde_json::from_str(line)?;
            if message.get("severity").and_then(serde_json::Value::as_str) != Some("information") {
                return Err(invalid("unexpected axiom diagnostic"));
            }
            let data = message
                .get("data")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| invalid("invalid axiom report"))?;
            let report = data
                .trim()
                .strip_prefix(&format!("'{name}' "))
                .ok_or_else(|| invalid("axiom report names differ from request"))?;
            if report == "does not depend on any axioms" {
                return Ok(Vec::new());
            }
            let axioms = report
                .strip_prefix("depends on axioms: [")
                .and_then(|s| s.strip_suffix(']'))
                .ok_or_else(|| invalid("incomplete axiom report"))?;
            axioms
                .split(',')
                .map(|axiom| {
                    let axiom = axiom.trim();
                    if axiom.is_empty() || axiom.contains(['[', ']', '\n', '\r']) {
                        return Err(invalid("invalid axiom name"));
                    }
                    Ok(axiom.to_owned())
                })
                .collect()
        })
        .collect::<io::Result<Vec<_>>>()?;
    if lines.next().is_some() {
        return Err(invalid("unexpected additional axiom output"));
    }
    Ok(reports)
}

pub(crate) fn operation(intent: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(intent).is_ok_and(|v| {
        v.get("operation")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|op| matches!(op, OPERATION | AXIOMS | MODULE_LINT | CHECKPOINT_LINT))
    })
}

pub(crate) fn recovery_request(key: NativeKey, intent: Vec<u8>, capability: Capability) -> Option<SavedRequest> {
    let parsed = serde_json::from_slice::<Intent>(&intent).ok()?;
    if !matches!(
        parsed.operation.as_str(),
        OPERATION | AXIOMS | MODULE_LINT | CHECKPOINT_LINT
    ) {
        return None;
    }
    Some(SavedRequest {
        caller: CallerKey {
            job: parsed.job,
            stage: key.stage,
            revision: parsed.revision,
        },
        key,
        intent,
        capability,
    })
}

pub(super) struct Stage {
    original: SavedRequest,
    deadline: Deadline,
    store: Arc<Mutex<NativeStore>>,
    cancellation: LeanWorkerCancellationToken,
    binding: Option<Binding>,
    generated: Option<Vec<u8>>,
}

impl Stage {
    pub(super) fn generated_input(&self) -> Option<&[u8]> {
        self.generated.as_deref()
    }

    pub(super) fn envelope(&self) -> io::Result<String> {
        Ok(serde_json::to_string(&self.deadline)?)
    }

    pub(super) fn release(
        &mut self,
        lease: &mut LeanWorkerNativeScopeLease,
        output: &mut Capture<std::process::ChildStdout>,
        cancelled: &(impl Fn() -> bool + ?Sized),
    ) -> io::Result<()> {
        loop {
            let budget = self.deadline.remaining()?;
            if cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let eof = output.poll()?;
            match output.bytes.as_slice() {
                [READY] => break,
                [] if !eof => thread::sleep(POLL.min(budget)),
                _ => return Err(invalid("external reader ready frame differs")),
            }
        }
        output.bytes.clear();
        let expected = lease.managed_guardian_binding()?;
        if uuid::Uuid::parse_str(&expected.boot)
            .map_err(io::Error::other)?
            .into_bytes()
            != self.deadline.boot
        {
            return Err(invalid("external reader Ready belongs to another boot"));
        }
        let registry = RegistryIdentity {
            directory: lean_toolchain::native_guardian::registry_directory()?
                .as_os_str()
                .as_bytes()
                .to_vec(),
            device: expected.registry_device,
            inode: expected.registry_inode,
            generation: expected.generation.clone(),
            boot: self.deadline.boot,
            parent: expected.parent.clone(),
            worker: expected.worker.clone(),
            guardian: expected.guardian.clone(),
        };
        let scope = Scope {
            id: uuid::Uuid::new_v4().into_bytes(),
            generation: 1,
            owner: uuid::Uuid::new_v4().into_bytes(),
        };
        let grant = {
            let mut store = self.store.lock();
            let original = store
                .client_get(self.original.key, &self.original.capability)
                .map_err(io::Error::other)?;
            if original.gate != Gate::Pending || original.scope.is_some() {
                return Err(invalid("external reader admission already spent or sealed"));
            }
            let binding = store
                .bind_timed_recovery_scope(&self.original, scope, &registry, self.deadline.expires_ns)
                .map_err(io::Error::other)?;
            self.binding = Some(binding);
            store
                .observe_resource(binding, &self.original.capability, ResourceReport::Open)
                .map_err(io::Error::other)?;
            let Consume::Granted(permit) = store
                .consume_launch(binding, &self.original.capability)
                .map_err(io::Error::other)?
            else {
                return Err(invalid("external reader did not consume the original gate"));
            };
            store
                .attach_active_stop(binding, self.cancellation.clone())
                .map_err(io::Error::other)?;
            drop(store);
            permit
                .into_deadline_grant()
                .ok_or_else(|| invalid("external reader deadline missing"))?
        };
        let (binding, identity, expires_ns, intent) = grant.into_parts();
        if Some(binding) != self.binding
            || identity != registry
            || expires_ns != self.deadline.expires_ns
            || intent != self.original.intent
            || cancelled()
        {
            return Err(invalid("external reader original grant changed or cancelled"));
        }
        if option_env!("LEAN_HOST_MCP_COMPILE_DISPOSABLE_CUT").is_some()
            && std::env::var_os("LEAN_HOST_MCP_TEST_READER_BEFORE_ARM").is_some_and(|path| Path::new(&path).is_file())
        {
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::STOP)?;
        }
        self.deadline.remaining()?;
        lease.arm_admitted_reader_deadline(&expected, binding.key.nonce, binding.key.stage, expires_ns)?;
        if option_env!("LEAN_HOST_MCP_COMPILE_DISPOSABLE_CUT").is_some()
            && std::env::var_os("LEAN_HOST_MCP_TEST_READER_AFTER_ARM").is_some_and(|path| Path::new(&path).is_file())
        {
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::STOP)?;
        }
        self.deadline.remaining()?;
        if cancelled() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let mut input = lease
            .take_stdin()
            .ok_or_else(|| invalid("external reader GO channel missing"))?;
        input.write_all(&[GO])?;
        Ok(())
    }

    fn finish(
        &self,
        outcome: io::Result<NativeReaderResult>,
        declarations: Option<&[String]>,
        lint: Option<module_lint::Request<'_>>,
    ) -> io::Result<()> {
        let Some(binding) = self.binding else {
            return outcome.map(|_| ());
        };
        let mut store = self.store.lock();
        store.detach_active_stop(binding);
        let outcome = outcome?;
        if outcome.scope_closed {
            store
                .observe_resource(binding, &self.original.capability, ResourceReport::Closed)
                .map_err(io::Error::other)?;
        }
        if outcome.state == "completed" && outcome.scope_closed {
            let mut body = serde_json::json!({
                "version":1, "exit_code":outcome.exit_code, "stdout":outcome.stdout,
                "stderr":outcome.stderr, "payload_usage":outcome.payload_usage,
            });
            if let Some(declarations) = declarations {
                let reports = if outcome.exit_code == Some(0) && outcome.stderr.is_empty() {
                    axiom_reports(&outcome.stdout, declarations).ok()
                } else {
                    None
                };
                if let (Some(reports), Some(fields)) = (reports, body.as_object_mut()) {
                    fields.insert(
                        "declarations".into(),
                        serde_json::json!(
                            declarations
                                .iter()
                                .zip(reports)
                                .map(|(name, axioms)| { serde_json::json!({"declaration":name, "axioms":axioms}) })
                                .collect::<Vec<_>>()
                        ),
                    );
                } else {
                    body = serde_json::json!({"version":1, "status":"axiom_reports_unavailable"});
                }
            }
            if let Some(lint) = lint {
                lint.record(&mut body, &outcome);
            }
            let body = serde_json::to_vec(&body)?;
            store
                .observe_result(
                    binding,
                    &self.original.capability,
                    uuid::Uuid::new_v4().into_bytes(),
                    &body,
                )
                .map_err(io::Error::other)?;
        }
        drop(store);
        Ok(())
    }
}

pub(crate) async fn start(
    broker: &ProjectBroker,
    store: &Arc<Mutex<NativeStore>>,
    key: NativeKey,
    capability: Capability,
) -> Result<Option<serde_json::Value>, McpError> {
    let invalid = |reason| McpError::invalid_params(reason, None);
    if capability.0 == [0; 32] {
        return Err(invalid("capability_denied"));
    }
    let original = store.lock().client_get(key, &capability);
    let Ok(original) = original else { return Ok(None) };
    if !operation(&original.intent) {
        return Ok(None);
    }
    if original.gate == Gate::Pending && original.scope.is_none() {
        let intent: Intent = serde_json::from_slice(&original.intent).map_err(|_| invalid("invalid_reader_intent"))?;
        if intent.job.is_empty() || intent.revision == 0 {
            return Err(invalid("invalid_reader_intent"));
        }
        let root = broker
            .native_root(&ProjectHint::Default)
            .map_err(|_| invalid("reader_project_unavailable"))?;
        if root != intent.project {
            return Err(invalid("reader_project_changed"));
        }
        let deadline = original
            .admission_deadline
            .ok_or_else(|| invalid("missing_admission_deadline"))?;
        deadline
            .remaining()
            .map_err(|_| invalid("admission_deadline_unavailable_or_expired"))?;
        let generated = match (
            intent.operation.as_str(),
            &intent.file,
            &intent.imports,
            &intent.declarations,
            &intent.modules,
            &intent.checkpoint,
        ) {
            (OPERATION, Some(_), None, None, None, None) => None,
            (AXIOMS, None, Some(imports), Some(declarations), None, None) => {
                Some(axiom_input(imports, declarations).map_err(|_| invalid("invalid_reader_intent"))?)
            }
            (MODULE_LINT, None, None, None, Some(modules), None) if module_lint::valid_modules(modules) => {
                module_lint::executable(&root).map_err(|_| invalid("prebuilt_module_linter_unavailable"))?;
                None
            }
            (CHECKPOINT_LINT, None, None, None, Some(modules), Some(checkpoint))
                if modules.len() == 1 && module_lint::valid_modules(modules) && checkpoint.valid() =>
            {
                module_lint::executable(&root).map_err(|_| invalid("prebuilt_module_linter_unavailable"))?;
                None
            }
            _ => return Err(invalid("invalid_reader_intent")),
        };
        let request = recovery_request(key, original.intent, capability.clone())
            .ok_or_else(|| invalid("invalid_reader_intent"))?;
        let store = Arc::clone(store);
        let max_workers = broker.config_snapshot().max_workers;
        tokio::task::spawn_blocking(move || {
            let mut stage = Stage {
                original: request,
                deadline: Deadline {
                    boot: deadline.boot,
                    expires_ns: deadline.expires_ns,
                },
                store,
                cancellation: LeanWorkerCancellationToken::new(),
                binding: None,
                generated,
            };
            let lint = intent.modules.as_deref().map(|modules| module_lint::Request {
                modules,
                checkpoint: intent.checkpoint.as_ref(),
            });
            let result = execute(&mut stage, &root, intent.file.as_deref(), lint, max_workers);
            stage.finish(result, intent.declarations.as_deref(), lint)
        })
        .await
        .map_err(|_| McpError::internal_error("reader_task_lost", None))?
        .map_err(|error| {
            tracing::debug!(%error, "admitted external reader did not confirm a result");
            McpError::internal_error("reader_result_unavailable", None)
        })?;
    }
    let query = serde_json::json!({"version":1,"command":{"op":"get","key":key,"capability":capability.0}});
    let mut store = store.lock();
    crate::managed_prepared::reconcile_lookup(&mut store, key, &capability);
    let status = store.admission(query.to_string().as_bytes()).map_err(invalid)?;
    drop(store);
    Ok(Some(crate::managed_prepared::advertise(status, broker)))
}

fn scan_file(root: &Path, relative: &Path) -> io::Result<PathBuf> {
    use std::path::Component;
    if relative.extension().is_none_or(|extension| extension != "lean")
        || relative.components().any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(invalid("external reader requires a project-relative Lean file"));
    }
    let file = root.join(relative);
    if file.canonicalize()? != file || !file.is_file() {
        return Err(invalid("external reader source alias or missing file"));
    }
    Ok(file)
}

fn execute(
    stage: &mut Stage,
    root: &Path,
    relative: Option<&Path>,
    lint: Option<module_lint::Request<'_>>,
    capacity_limit: usize,
) -> io::Result<NativeReaderResult> {
    let selected = ToolchainId::from_lake_root(root).map_err(io::Error::other)?;
    if selected.window_verdict() != WindowVerdict::Supported {
        return Err(invalid("unsupported external reader toolchain"));
    }
    let sysroot = selected.elan_dir().map_err(io::Error::other)?.canonicalize()?;
    let lean = sysroot.join("bin/lean");
    if lean.canonicalize()? != lean || !lean.is_file() {
        return Err(invalid("external reader executable is not canonical"));
    }
    let helper = crate::shared_writer::contract()?;
    let artifact_roots = lean_toolchain::shared_writer::project_artifact_roots(root)?;
    let lean_path = std::env::join_paths(
        artifact_roots
            .iter()
            .map(|root| root.join(crate::lake_meta::BUILD_LIB_REL)),
    )
    .map_err(io::Error::other)?;
    let program = NativeReaderProgram {
        command: match (relative, lint) {
            (Some(file), _) => vec![
                lean.into_os_string(),
                "--deps".into(),
                scan_file(root, file)?.into_os_string(),
            ],
            (None, Some(request)) => request.command(root)?,
            (None, None) => vec![lean.into_os_string(), "--json".into(), "--stdin".into()],
        },
        environment: vec![
            ("LEAN_SYSROOT".into(), sysroot.into_os_string()),
            ("LEAN_PATH".into(), lean_path),
        ],
        cwd: root.to_owned(),
        artifact_roots: Vec::new(),
    };
    let budget = stage.deadline.remaining()?;
    let cancellation = stage.cancellation.clone();
    let original = stage.deadline;
    let cancelled = || cancellation.is_cancelled() || original.remaining().is_err();
    let deadline = until(budget)?;
    let scope = reserve_scope(&helper, capacity_limit, true, deadline, &cancelled)?;
    let args = NativeReaderArgs {
        project_root: root.to_owned(),
        artifact_root: Vec::new(),
        timeout_millis: u64::try_from(budget.as_millis()).map_err(io::Error::other)?,
        capacity_limit: u16::try_from(capacity_limit).map_err(io::Error::other)?,
        stdin: stage.generated_input().is_some(),
        cancel_fd: None,
        stdout_result_frame: true,
        max_output_bytes: if relative.is_some() { 4096 } else { 16 * 1024 },
        command: program.command.clone(),
    };
    let mut spawned = false;
    run_internal(
        &args,
        Some(&program),
        Some(&helper),
        Some((scope, deadline)),
        Some(stage),
        None,
        &cancelled,
        &mut spawned,
    )
}
