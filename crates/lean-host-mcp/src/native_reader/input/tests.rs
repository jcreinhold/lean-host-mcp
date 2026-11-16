//! Byte delivery and descriptor-bound refusal, without a native runtime.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::os::unix::fs::symlink;

fn deadline() -> Instant {
    Instant::now().checked_add(Duration::from_secs(2)).unwrap()
}

#[test]
fn captured_eof_is_the_exact_readonly_payload_descriptor() {
    let owner = tempfile::tempdir().unwrap();
    let owner = owner.path().canonicalize().unwrap();
    let expected = "∀ n, n = n\0\n".as_bytes();
    let mut input = tempfile::tempfile().unwrap();
    input.write_all(expected).unwrap();
    input.rewind().unwrap();
    let staged = capture(&input, &owner, deadline(), &|| Ok(true)).unwrap();
    assert_eq!(staged.bytes, expected.len() as u64);
    let mut actual = open_verified(&staged).unwrap();
    assert!(actual.write_all(b"changed").is_err(), "payload fd0 is read-only");
    let mut bytes = Vec::new();
    actual.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, expected);
}

#[test]
fn delivery_without_eof_has_one_deadline_and_restores_flags() {
    let directory = tempfile::tempdir().unwrap();
    let (input, _writer) = std::io::pipe().unwrap();
    let flags = fcntl_getfl(&input).unwrap();
    let start = Instant::now();
    let error = capture(
        &input,
        directory.path(),
        start.checked_add(Duration::from_millis(30)).unwrap(),
        &|| Ok(true),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(fcntl_getfl(&input).unwrap(), flags);
    assert!(!directory.path().join("stdin").exists());
}

#[test]
fn cancelled_delivery_creates_no_input_asset() {
    let directory = tempfile::tempdir().unwrap();
    let input = tempfile::tempfile().unwrap();
    assert_eq!(
        capture(&input, directory.path(), deadline(), &|| Ok(false))
            .unwrap_err()
            .kind(),
        io::ErrorKind::Interrupted
    );
    assert!(!directory.path().join("stdin").exists());
}

#[test]
fn oversized_delivery_refuses_without_publishing_partial_input() {
    let directory = tempfile::tempdir().unwrap();
    let mut input = tempfile::tempfile().unwrap();
    input.set_len((MAX_BYTES as u64) + 1).unwrap();
    input.rewind().unwrap();
    assert_eq!(
        capture(&input, directory.path(), deadline(), &|| Ok(true))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert!(!directory.path().join("stdin").exists());
}

#[test]
fn changed_input_and_aliases_refuse_before_payload() {
    let directory = tempfile::tempdir().unwrap();
    let owner = directory.path().canonicalize().unwrap();
    let mut input = tempfile::tempfile().unwrap();
    input.write_all(b"original").unwrap();
    input.rewind().unwrap();
    let staged = capture(&input, &owner, deadline(), &|| Ok(true)).unwrap();
    std::fs::write(&staged.path, b"modified").unwrap();
    assert!(open_verified(&staged).is_err());
    std::fs::remove_file(&staged.path).unwrap();
    let sentinel = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(sentinel.path(), b"external").unwrap();
    symlink(sentinel.path(), &staged.path).unwrap();
    assert!(open_verified(&staged).is_err());
    assert_eq!(std::fs::read(sentinel.path()).unwrap(), b"external");
}

#[test]
fn shared_socket_input_is_refused_without_changing_flags() {
    let directory = tempfile::tempdir().unwrap();
    let (input, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let flags = fcntl_getfl(&input).unwrap();
    assert_eq!(
        capture(&input, directory.path(), deadline(), &|| Ok(true))
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(fcntl_getfl(&input).unwrap(), flags);
    assert!(!directory.path().join("stdin").exists());
}
