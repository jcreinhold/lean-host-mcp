//! Preserve semantic runtime writer evidence through its error chain.

#![allow(clippy::expect_used)]

use std::io;

use lean_toolchain::SourcePackageError;
use lean_toolchain::shared_writer::{WriterDisposition, WriterFailure};
use serde_json::json;

use super::*;

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
    lean_semantic_search_runtime::Error::from(SourcePackageError::Io {
        action: "source writer transaction",
        path: PathBuf::from("/unused-semantic-source"),
        source: io::Error::new(io::ErrorKind::Interrupted, writer_failure(disposition)),
    })
}

#[test]
fn runtime_error_chains_preserve_writer_evidence() {
    for disposition in [
        WriterDisposition::NotStarted,
        WriterDisposition::Stopped,
        WriterDisposition::Unknown,
    ] {
        // The runtime's writer step keeps the evidence in an `io::Error` that its
        // opaque `Error` wraps. Only the runtime can build that wrapper, and its
        // own tests check it, so this checks the `io::Error` itself.
        let failures = [
            SemanticRuntimeFailure::from_error(source_failure(disposition)),
            SemanticRuntimeFailure::from_error(io::Error::other(writer_failure(disposition))),
        ];
        for failure in failures {
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

#[test]
fn a_missing_semantic_cache_directory_is_created_and_a_bad_one_is_named() {
    let directory = tempfile::tempdir().expect("scratch directory");
    let root = directory.path().canonicalize().expect("canonical scratch");
    let missing = root.join("semantic").join("cache");
    assert_eq!(env_cache_dir(missing.clone()).expect("created"), missing);
    assert!(missing.is_dir());

    let file = root.join("file");
    std::fs::write(&file, "not a directory").expect("file");
    let error = env_cache_dir(file.join("cache")).expect_err("under a file");
    assert!(error.to_string().contains(&file.display().to_string()), "{error}");

    let relative = env_cache_dir(PathBuf::from("relative-cache")).expect_err("relative");
    assert!(relative.to_string().contains("canonical absolute path"), "{relative}");
}
