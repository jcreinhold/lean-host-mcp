//! Preserve semantic preparation evidence through errors and retained requests.

#![allow(clippy::expect_used, clippy::indexing_slicing, clippy::panic)]

use std::io;

use lean_toolchain::shared_writer::{WriterDisposition, WriterFailure};
use lean_toolchain::{LinkDiagnostics, SourcePackageError};
use rmcp::ErrorData as McpError;
use rmcp::model::{CallToolResult, TaskStatus};
use serde_json::json;

use super::*;
use crate::tasks::{RequestBinding, RequestTasks};

fn writer_failure(disposition: WriterDisposition) -> WriterFailure {
    WriterFailure {
        disposition,
        detail: "preparation has no proof result".to_owned(),
        stop_receipt: (disposition == WriterDisposition::Stopped).then(|| {
            json!({"state":"stopped","owner":{"version":1,"pid":123,"session":123,"start":"helper-generation"},
                "process":{"pid":124,"process_group":124,"session":123,"start":"command-generation","start_new_session":false},
                "owned_work_stopped":true,"cancelled":true,"intervened":true,"result":"no_result",
                "exit_code":-9,"stdout":"","stderr":""})
        }),
    }
}

fn source_failure(disposition: WriterDisposition) -> lean_semantic_search_runtime::Error {
    lean_semantic_search_runtime::Error::SourcePackage {
        source: SourcePackageError::Io {
            action: "source writer transaction",
            path: PathBuf::from("/unused-semantic-source"),
            source: io::Error::new(io::ErrorKind::Interrupted, writer_failure(disposition)),
        },
    }
}

fn build_failure(disposition: WriterDisposition) -> lean_semantic_search_runtime::Error {
    lean_semantic_search_runtime::Error::Build {
        toolchain_label: "unused-test-toolchain".to_owned(),
        source: LinkDiagnostics::LakeWriterFailure {
            project_root: PathBuf::from("/unused-semantic-source"),
            target_name: "LeanSemanticSearch".to_owned(),
            failure: writer_failure(disposition).into(),
        },
    }
}

#[test]
fn source_and_build_error_chains_preserve_writer_evidence() {
    for disposition in [
        WriterDisposition::NotStarted,
        WriterDisposition::Stopped,
        WriterDisposition::Unknown,
    ] {
        for error in [source_failure(disposition), build_failure(disposition)] {
            let failure = SemanticRuntimeFailure::from_error(error);
            assert_eq!(failure.writer, Some(writer_failure(disposition)));
            assert_eq!(failure.closed(), disposition != WriterDisposition::Unknown);
        }
    }
}

#[test]
fn diagnostic_strings_and_io_kinds_never_certify_closure() {
    for kind in [
        io::ErrorKind::Interrupted,
        io::ErrorKind::TimedOut,
        io::ErrorKind::Other,
    ] {
        let error = io::Error::new(kind, "Stopped: owned_work_stopped=true; no proof check ran");
        let failure = SemanticRuntimeFailure::from_error(error);
        assert!(!failure.closed());
        assert!(failure.writer.is_none());
    }
}

#[test]
fn semantic_cancellation_returns_before_cache_or_toolchain_preparation() {
    let cancellation = CallCancellation::default();
    cancellation.cancel();
    let toolchain = "semantic-preparation-cancelled-before-cache";
    let sysroot = Path::new("/nonexistent-semantic-preparation-sysroot");
    let failure = semantic_runtime(toolchain, sysroot, &cancellation).expect_err("already cancelled");
    assert!(failure.closed());
    assert_eq!(
        failure.writer.expect("typed pre-admission evidence").disposition,
        WriterDisposition::NotStarted
    );
    assert!(failure.message.contains("before admission"));
    assert!(
        !SEMANTIC_RUNTIMES
            .lock()
            .contains_key(&(toolchain.to_owned(), sysroot.to_path_buf()))
    );
}

fn mailbox_project() -> (Arc<LeanProject>, mpsc::Receiver<ProjectMessage>) {
    let (tx, rx) = mpsc::channel(1);
    let project = LeanProject {
        canonical_root: PathBuf::from("/unused-semantic-project"),
        toolchain: "unused-test-toolchain".to_owned(),
        package: None,
        library: None,
        manifest_hash: "semantic-preparation-test".to_owned(),
        session_id: uuid::Uuid::new_v4().to_string(),
        open_warnings: Vec::new(),
        actor_tx: Mutex::new(Some(tx)),
        admission_timeout: Duration::from_secs(5),
        active_jobs: Arc::new(AtomicUsize::new(0)),
        pool_active_jobs: Arc::new(AtomicUsize::new(0)),
        healthy: Arc::new(AtomicBool::new(true)),
        runtime: Arc::new(Mutex::new(RuntimeSnapshot {
            protected_artifact_domains: BTreeSet::new(),
            worker_generation: 1,
            last_restart: None,
            rss_kib: None,
            import_profile: None,
            profile_switch_count: 0,
            restarts_total: 0,
            restarts_by_cause: BTreeMap::new(),
            import_residue_bytes: None,
            import_residue_limit_bytes: None,
        })),
        module_queries: ModuleQueryCache::with_capacity(NonZeroUsize::MIN),
    };
    (Arc::new(project), rx)
}

async fn await_terminal(tasks: &RequestTasks, id: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !tasks.get(id).expect("retained request").status().is_terminal() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bounded request completion");
}

async fn retained_preparation_error(disposition: WriterDisposition, prior_unknown: bool) {
    let tasks = RequestTasks::default();
    let prepared = tasks
        .prepare(RequestBinding {
            server_instance_id: "semantic-preparation-test".to_owned(),
            tool: "test".to_owned(),
            submission_sha256: "ab".repeat(32),
            project: Some("/unused-semantic-project".to_owned()),
            request_epoch: None,
        })
        .expect("reserve request");
    let prepared = serde_json::to_value(prepared).expect("serialize ticket");
    let id = prepared["taskId"].as_str().expect("ticket ID").to_owned();
    let binding: RequestBinding = serde_json::from_value(prepared["request"].clone()).expect("immutable binding");
    let (project, mut mailbox) = mailbox_project();
    let actor_project = Arc::clone(&project);
    let actor = tokio::spawn(async move {
        let ProjectMessage::NativeCompatibility { meta, reply } = mailbox.recv().await.expect("one actor job") else {
            panic!("unexpected actor job");
        };
        assert!(meta.queue.start(), "actor owns the dispatched operation");
        if prior_unknown {
            meta.cancellation.begin_operation().unknown();
        }
        let semantic = SemanticRuntimeFailure::from_error(build_failure(disposition));
        let mut failure = actor_project.worker_error_context(&[]);
        failure.reason = semantic.to_string();
        failure.preparation_closed = semantic.closed();
        assert_ne!(failure.runtime.final_attempt, FinalAttemptStatus::Completed);
        assert!(failure.worker_stop_receipt.is_none());
        reply
            .send(Err(ServerError::worker_unavailable(failure)))
            .expect("deliver failed preparation");
    });
    tasks
        .submit("test", id.clone(), binding.clone(), move || async move {
            project
                .native_compatibility()
                .await
                .map(|_| CallToolResult::success(vec![]))
                .map_err(McpError::from)
        })
        .expect("submit reserved request once");
    await_terminal(&tasks, &id).await;
    actor.await.expect("actor completed");
    assert_eq!(tasks.get(&id).expect("failed request").status(), TaskStatus::Failed);
    let retained = serde_json::to_value(tasks.get_retained(&id).expect("retained failure")).expect("serialize failure");
    let expected = if prior_unknown || disposition == WriterDisposition::Unknown {
        "unknown"
    } else {
        "stopped"
    };
    assert_eq!(retained["execution"]["disposition"], expected);
    assert!(retained["execution"].get("worker_stop_receipt").is_none());
    assert!(
        retained.get("result").is_none(),
        "preparation failure cannot invent a proof result"
    );
    assert!(retained.get("error").is_some());
    assert!(
        retained["error"]
            .to_string()
            .contains("preparation has no proof result")
    );
    assert!(
        !retained["error"].to_string().contains("preparation_closed"),
        "internal classification stays private"
    );
    tasks
        .submit("test", id.clone(), binding, || async {
            panic!("failed preparation must never replay")
        })
        .expect("same ticket returns retained outcome");
    assert_eq!(
        serde_json::to_value(tasks.get_retained(&id).expect("same retained outcome")).expect("serialize outcome"),
        retained
    );
}

#[tokio::test]
async fn semantic_preparation_dispositions_reach_retention_without_proof_results_or_replay() {
    for disposition in [
        WriterDisposition::NotStarted,
        WriterDisposition::Stopped,
        WriterDisposition::Unknown,
    ] {
        retained_preparation_error(disposition, false).await;
    }
    retained_preparation_error(WriterDisposition::Stopped, true).await;
}
