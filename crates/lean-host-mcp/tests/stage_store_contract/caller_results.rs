use super::*;
use crate::result_client::accept_then_ack;
use sha2::{Digest, Sha256};

const BODY: &[u8] = b"caller-owned output";
fn published(
    journal: &mut CallerJournal,
    store: &mut NativeStore,
    stage: u64,
    generation: u64,
    body: &[u8],
) -> (SavedRequest, Binding) {
    let (request, binding) = bound(journal, store, stage, generation);
    assert!(granted(store.consume_launch(binding, &request.capability).unwrap()));
    store
        .observe_resource(binding, &request.capability, ResourceReport::Open)
        .unwrap();
    store
        .observe_spawn(binding, &request.capability, Spawn::Spawned)
        .unwrap();
    store
        .observe_result(binding, &request.capability, [42; 16], body)
        .unwrap();
    (request, binding)
}
fn assert_copy(copy: &CallerResult, request: &SavedRequest, binding: Binding, body: &[u8]) {
    assert_eq!(copy.caller(), &request.caller);
    assert_eq!(copy.binding(), binding);
    assert_eq!(copy.result_id(), [42; 16]);
    assert_eq!(copy.body(), body);
    assert_eq!(copy.hash(), Sha256::digest(body).as_slice());
}
fn native_fresh(fixture: &Fixture, key: NativeKey) -> Snapshot {
    NativeStore::open(&fixture.native()).unwrap().get(key).unwrap().unwrap()
}
fn caller_trigger(fixture: &Fixture) {
    let checked = CheckedConnection::open_existing(&fixture.caller(), caller_schema_valid).unwrap();
    checked.connection().execute_batch("CREATE TRIGGER main.reject_caller_result BEFORE INSERT ON accepted_results BEGIN SELECT RAISE(ABORT,'caller result insert rejected'); END;").unwrap();
}
fn assert_sql_rejection(error: &Problem, message: &str) {
    match error {
        Problem::Sql(rusqlite::Error::SqliteFailure(code, Some(actual))) => {
            assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER);
            assert_eq!(actual, message);
        }
        other @ (Problem::Sql(_) | Problem::Connection(_) | Problem::Rejected(_)) => {
            panic!("expected actual SQLite rejection: {other:?}")
        }
    }
}

#[test]
fn caller_copy_survives_ack_native_expiry_and_cold_reopen() {
    let fixture = Fixture::new();
    let (mut journal, mut store) = fixture.init();
    let (request, binding) = published(&mut journal, &mut store, 1, 0, BODY);
    let before = store.get(request.key).unwrap().unwrap();
    assert!(journal.saved_result(&request.caller).unwrap().is_none());
    let accepted = accept_then_ack(&mut journal, &mut store, &request, binding).unwrap();
    assert_copy(&accepted, &request, binding, BODY);
    assert!(matches!(
        store.get(request.key).unwrap().unwrap().result,
        ResultKnowledge::Acknowledged { .. }
    ));
    store.expire(binding, &request.capability, [42; 16]).unwrap();
    drop(journal);
    let mut journal = CallerJournal::open(&fixture.caller()).unwrap();
    assert_eq!(journal.saved_result(&request.caller).unwrap(), Some(accepted.clone()));
    assert_eq!(
        accept_then_ack(&mut journal, &mut store, &request, binding).unwrap(),
        accepted
    );
    let after = store.get(request.key).unwrap().unwrap();
    assert!(matches!(after.result, ResultKnowledge::Expired { .. }));
    assert_eq!(
        (after.gate, after.spawn, after.resource_report, after.stop_requested),
        (before.gate, before.spawn, before.resource_report, before.stop_requested)
    );
    drop(journal);
    drop(store);
    let cold = CallerJournal::open(&fixture.caller())
        .unwrap()
        .saved_result(&request.caller)
        .unwrap()
        .unwrap();
    assert_copy(&cold, &request, binding, BODY);
}

#[test]
fn caller_copy_rejects_changed_request_scope_result_and_bytes() {
    let fixture = Fixture::new();
    let (mut journal, mut store) = fixture.init();
    let (request, binding) = published(&mut journal, &mut store, 1, 0, BODY);
    let source = store.get(request.key).unwrap().unwrap();
    let accepted = journal.save_result(&request, &source).unwrap();
    assert_eq!(journal.save_result(&request, &source).unwrap(), accepted);
    let mut changed = request.clone();
    changed.intent.push(b'!');
    assert!(matches!(
        journal.save_result(&changed, &source),
        Err(Problem::Rejected(Reject::CallerBindingChanged))
    ));
    changed = request.clone();
    changed.capability = Capability([99; 32]);
    assert!(matches!(
        journal.save_result(&changed, &source),
        Err(Problem::Rejected(Reject::CallerBindingChanged))
    ));
    let mut wrong = source.clone();
    wrong.scope = Some(Scope {
        generation: 1,
        ..binding.scope
    });
    assert!(matches!(
        journal.save_result(&request, &wrong),
        Err(Problem::Rejected(Reject::ConflictingCallerResult))
    ));
    wrong = source.clone();
    wrong.result = ResultKnowledge::Retained {
        id: [11; 16],
        hash: Sha256::digest(BODY).to_vec(),
        body: BODY.to_vec(),
    };
    assert!(matches!(
        journal.save_result(&request, &wrong),
        Err(Problem::Rejected(Reject::ConflictingCallerResult))
    ));
    wrong = source.clone();
    wrong.result = ResultKnowledge::Retained {
        id: [42; 16],
        hash: Sha256::digest(b"different").to_vec(),
        body: b"different".to_vec(),
    };
    assert!(matches!(
        journal.save_result(&request, &wrong),
        Err(Problem::Rejected(Reject::ConflictingCallerResult))
    ));
    wrong = source.clone();
    wrong.result = ResultKnowledge::Retained {
        id: [42; 16],
        hash: vec![0; 32],
        body: BODY.to_vec(),
    };
    assert!(matches!(
        journal.save_result(&request, &wrong),
        Err(Problem::Rejected(Reject::InvalidResult))
    ));
    assert_eq!(journal.saved_result(&request.caller).unwrap(), Some(accepted));
    assert_eq!(native_fresh(&fixture, request.key), source);
}

#[test]
fn client_ack_never_targets_a_newer_task_with_the_same_result_id() {
    let fixture = Fixture::new();
    let (mut journal, mut store) = fixture.init();
    let (old, b1) = published(&mut journal, &mut store, 1, 0, BODY);
    let (new, b2) = published(&mut journal, &mut store, 2, 1, b"new output");
    let newer = native_fresh(&fixture, new.key);
    let older = native_fresh(&fixture, old.key);
    assert!(matches!(
        accept_then_ack(&mut journal, &mut store, &old, b2),
        Err(Problem::Rejected(Reject::CallerBindingChanged))
    ));
    let mixed = Binding { scope: b2.scope, ..b1 };
    assert!(matches!(
        accept_then_ack(&mut journal, &mut store, &old, mixed),
        Err(Problem::Rejected(Reject::WrongScope))
    ));
    let mut wrong = old.clone();
    wrong.caller = new.caller.clone();
    assert!(matches!(
        accept_then_ack(&mut journal, &mut store, &wrong, b1),
        Err(Problem::Rejected(Reject::CallerBindingChanged))
    ));
    assert_eq!(native_fresh(&fixture, new.key), newer);
    assert_eq!(native_fresh(&fixture, old.key), older);
    assert!(journal.saved_result(&old.caller).unwrap().is_none());
    assert!(journal.saved_result(&new.caller).unwrap().is_none());
    accept_then_ack(&mut journal, &mut store, &old, b1).unwrap();
    assert_eq!(native_fresh(&fixture, new.key), newer);
}

#[test]
fn expired_native_bytes_without_a_caller_copy_are_unavailable() {
    let fixture = Fixture::new();
    let (mut journal, mut store) = fixture.init();
    let (request, binding) = published(&mut journal, &mut store, 1, 0, BODY);
    store.expire(binding, &request.capability, [42; 16]).unwrap();
    let before = native_fresh(&fixture, request.key);
    assert!(matches!(
        accept_then_ack(&mut journal, &mut store, &request, binding),
        Err(Problem::Rejected(Reject::ResultUnavailable))
    ));
    assert!(journal.saved_result(&request.caller).unwrap().is_none());
    assert_eq!(native_fresh(&fixture, request.key), before);
    assert_eq!(journal.get(&request.caller).unwrap(), Some(request));
}

#[test]
fn caller_insert_rejection_prevents_native_ack() {
    let fixture = Fixture::new();
    let (mut journal, mut store) = fixture.init();
    let (request, binding) = published(&mut journal, &mut store, 1, 0, BODY);
    let before = native_fresh(&fixture, request.key);
    caller_trigger(&fixture);
    let error = accept_then_ack(&mut journal, &mut store, &request, binding).unwrap_err();
    assert_sql_rejection(&error, "caller result insert rejected");
    assert_eq!(
        native_fresh(&fixture, request.key),
        before,
        "native ACK preceded durable caller result commit"
    );
    let cold = CallerJournal::open(&fixture.caller()).unwrap();
    assert!(cold.saved_result(&request.caller).unwrap().is_none());
    assert_eq!(cold.get(&request.caller).unwrap(), Some(request));
}

#[test]
fn native_ack_rejection_preserves_caller_copy_for_retry_after_expiry() {
    let fixture = Fixture::new();
    let (mut journal, mut store) = fixture.init();
    let (request, binding) = published(&mut journal, &mut store, 1, 0, BODY);
    let before = native_fresh(&fixture, request.key);
    let fault = CheckedConnection::open_existing(&fixture.native(), native_schema_valid).unwrap();
    fault.connection().execute_batch("CREATE TRIGGER main.reject_ack BEFORE UPDATE ON stages WHEN NEW.result_kind=2 BEGIN SELECT RAISE(ABORT,'native ACK rejected'); END;").unwrap();
    drop(fault);
    let error = accept_then_ack(&mut journal, &mut store, &request, binding).unwrap_err();
    assert_sql_rejection(&error, "native ACK rejected");
    let accepted = CallerJournal::open(&fixture.caller())
        .unwrap()
        .saved_result(&request.caller)
        .unwrap()
        .unwrap();
    assert_copy(&accepted, &request, binding, BODY);
    assert_eq!(native_fresh(&fixture, request.key), before);
    store.expire(binding, &request.capability, [42; 16]).unwrap();
    drop(journal);
    let mut journal = CallerJournal::open(&fixture.caller()).unwrap();
    assert_eq!(
        accept_then_ack(&mut journal, &mut store, &request, binding).unwrap(),
        accepted
    );
    assert!(matches!(
        native_fresh(&fixture, request.key).result,
        ResultKnowledge::Expired { .. }
    ));
}

#[test]
fn caller_result_and_ack_kill_cuts_preserve_two_commit_ordering() {
    use std::os::unix::process::ExitStatusExt;
    for operation in ["caller-result", "native-ack"] {
        for committed in [false, true] {
            for checkpointed in [false, true] {
                let fixture = Fixture::new();
                let (mut journal, mut store) = fixture.init();
                let (request, binding) = published(&mut journal, &mut store, 1, 0, BODY);
                let before = native_fresh(&fixture, request.key);
                if checkpointed {
                    store.checkpoint().unwrap();
                    let checked = CheckedConnection::open_existing(&fixture.caller(), caller_schema_valid).unwrap();
                    let row: (i64, i64, i64) = checked
                        .connection()
                        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                        })
                        .unwrap();
                    assert_eq!(row.0, 0);
                }
                let marker = fixture.root.join("cut-ready");
                let point = format!("{operation}:{}", if committed { "after" } else { "before" });
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", "tests::process_cut_helper", "--nocapture"])
                    .env("LEAN_STAGE_STORE_FIXTURE", &fixture.root)
                    .env("LEAN_STAGE_STORE_OPERATION", "accept-and-ack")
                    .env("LEAN_STAGE_STORE_CUT", &point)
                    .env("LEAN_STAGE_STORE_READY", &marker)
                    .stdin(Stdio::null())
                    .stdout(fs::File::create(fixture.root.join("helper.stdout")).unwrap())
                    .stderr(fs::File::create(fixture.root.join("helper.stderr")).unwrap());
                let mut child = ChildOwner(Some(command.spawn().unwrap()));
                wait_for_cut(&mut child, &marker, &point, &fixture.root);
                child.0.as_mut().unwrap().kill().unwrap();
                let exit = child.0.as_mut().unwrap().wait().unwrap();
                assert_eq!(exit.signal(), Some(9));
                child.0 = None;
                let mut caller = CallerJournal::open(&fixture.caller()).unwrap();
                let mut native = NativeStore::open(&fixture.native()).unwrap();
                let saved = caller.saved_result(&request.caller).unwrap();
                let exists = operation == "native-ack" || committed;
                assert_eq!(saved.is_some(), exists);
                if let Some(copy) = saved {
                    assert_copy(&copy, &request, binding, BODY);
                }
                let mut expected = before;
                if operation == "native-ack" && committed {
                    expected.result = ResultKnowledge::Acknowledged {
                        id: [42; 16],
                        hash: Sha256::digest(BODY).to_vec(),
                        body: BODY.to_vec(),
                    };
                }
                assert_eq!(native.get(request.key).unwrap(), Some(expected));
                let recovered = caller.get(&request.caller).unwrap().unwrap();
                assert_eq!(recovered, request);
                let accepted = accept_then_ack(&mut caller, &mut native, &recovered, binding).unwrap();
                assert_copy(&accepted, &request, binding, BODY);
                native.expire(binding, &request.capability, [42; 16]).unwrap();
                native.checkpoint().unwrap();
                drop(caller);
                drop(native);
                drop(store);
                drop(journal);
                let cold = CallerJournal::open(&fixture.caller())
                    .unwrap()
                    .saved_result(&request.caller)
                    .unwrap()
                    .unwrap();
                assert_copy(&cold, &request, binding, BODY);
                assert!(matches!(
                    native_fresh(&fixture, request.key).result,
                    ResultKnowledge::Expired { .. }
                ));
                eprintln!("caller-result ordering cut passed: {point}, baseline checkpointed={checkpointed}");
            }
        }
    }
}
