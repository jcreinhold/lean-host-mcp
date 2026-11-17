//! Test-only database permission -> original one-shot dummy runtime.
#![cfg(all(test, target_os = "macos"))]
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "disposable native fixture checks exact original observations"
)]
#![allow(
    dead_code,
    reason = "this target exercises native callers; database-only tests cover remaining store operations"
)]
#[path = "stage_store_contract/connection.rs"]
mod connection;
#[path = "stage_runtime_contract/fixtures.rs"]
mod fixtures;
#[path = "../../../specs/native-execution/stage_kernel.rs"]
mod kernel;
#[path = "stage_store_contract/result_client.rs"]
mod result_client;
#[path = "stage_runtime_contract/runtime.rs"]
mod runtime;
#[path = "stage_store_contract/store.rs"]
mod store;
use fixtures::{Contender, Dispatch, Fixture, GoFault, ObservedKind};
use kernel::{AttemptId, ScopeBinding, StageBinding, StageKey};
use runtime::{PollFault, Runtime};
use store::{
    Binding, CallerJournal, CallerKey, Capability, Consume, Gate, NativeKey, NativeStore, ResourceReport,
    ResultKnowledge, SavedRequest, Scope,
};

// Commit/crash hooks are exercised in the database-only target. Delivery loss
// uses fixture switches; SQL rejection below comes from an actual SQLite trigger.
fn cut(_point: &str) {}
struct Mapping {
    key: StageKey,
    scope: ScopeBinding,
    sql: Binding,
    request: SavedRequest,
}
fn establish(fixture: &Fixture, journal: &mut CallerJournal, store: &mut NativeStore) -> Mapping {
    assert_eq!(fixture.root_starts(), 0);
    let caller = CallerKey {
        job: b"coupled original job".to_vec(),
        stage: 1,
        revision: 1,
    };
    let key = NativeKey {
        store: store.identity(),
        epoch: store.epoch().unwrap(),
        nonce: uuid::Uuid::new_v4().into_bytes(),
        stage: 1,
    };
    let request = journal
        .bind(caller, b"actual dummy input", key, Capability([7; 32]))
        .unwrap();
    store.create(&request).unwrap();
    let sql_scope = Scope {
        id: uuid::Uuid::new_v4().into_bytes(),
        generation: 0,
        owner: uuid::Uuid::new_v4().into_bytes(),
    };
    let sql = store.bind_scope(request.key, &request.capability, sql_scope).unwrap();
    // Convert ONCE before native preparation; delayed messages keep this binding.
    let key = StageKey::new(
        AttemptId::new(
            u128::from_be_bytes(request.key.store),
            request.key.epoch,
            u128::from_be_bytes(request.key.nonce),
        ),
        request.key.stage,
    );
    let scope = ScopeBinding::new(
        u128::from_be_bytes(sql_scope.id),
        sql_scope.generation,
        u128::from_be_bytes(sql_scope.owner),
    );
    assert_eq!(fixture.root_starts(), 0); // Both stores committed before any root.
    Mapping {
        key,
        scope,
        sql,
        request,
    }
}
#[derive(Debug)]
enum LiveError {
    IdentityMismatch,
    Authorization(kernel::Rejected),
    Storage(store::Problem),
}
impl std::fmt::Display for LiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IdentityMismatch => f.write_str("SQL and live task identities differ"),
            Self::Authorization(error) => write!(f, "local launch rejected: {error:?}"),
            Self::Storage(error) => error.fmt(f),
        }
    }
}

fn validate_original(runtime: &Runtime, binding: StageBinding, mapping: &Mapping) -> Result<(), LiveError> {
    let sql_key = mapping.sql.key;
    let sql_scope = mapping.sql.scope;
    let expected_key = StageKey::new(
        AttemptId::new(
            u128::from_be_bytes(sql_key.store),
            sql_key.epoch,
            u128::from_be_bytes(sql_key.nonce),
        ),
        sql_key.stage,
    );
    let expected_scope = ScopeBinding::new(
        u128::from_be_bytes(sql_scope.id),
        sql_scope.generation,
        u128::from_be_bytes(sql_scope.owner),
    );
    if mapping.request.key != sql_key
        || mapping.key != expected_key
        || mapping.scope != expected_scope
        || runtime.scope().binding != mapping.scope
        || runtime.stage(mapping.key).map(|stage| stage.binding) != Some(binding)
    {
        return Err(LiveError::IdentityMismatch);
    }
    Ok(())
}
fn stop_original(runtime: &mut Runtime, binding: StageBinding) {
    // Cleanup needs no successful database write.
    if let Some(stop) = runtime.request_cancel(binding).unwrap() {
        runtime.deliver_stop(&stop, false);
    }
}
// This error route cannot require a successful database call: it has no store.
fn storage_failed(runtime: &mut Runtime, binding: StageBinding, error: store::Problem) -> LiveError {
    stop_original(runtime, binding);
    LiveError::Storage(error)
}
fn start_original(
    runtime: &mut Runtime,
    binding: StageBinding,
    store: &mut NativeStore,
    mapping: &Mapping,
    fault: GoFault,
) -> Result<Dispatch, LiveError> {
    validate_original(runtime, binding, mapping)?;
    let Some(packet) = runtime.request_start(binding).map_err(LiveError::Authorization)? else {
        return Ok(Dispatch::AlreadyTaken);
    };
    let delivery = match store.consume_launch(mapping.sql, &mapping.request.capability) {
        Ok(Consume::Granted(permit)) => {
            if permit.into_binding() != mapping.sql {
                stop_original(runtime, binding);
                return Err(LiveError::IdentityMismatch);
            }
            runtime.deliver_launch(&packet, fault)
        }
        Ok(Consume::AlreadyConsumed | Consume::Sealed) => {
            stop_original(runtime, binding);
            return Ok(Dispatch::NotSent);
        }
        Err(error) => return Err(storage_failed(runtime, binding, error)),
    };
    Ok(delivery)
}
fn wait_native_closed(runtime: &mut Runtime, binding: StageBinding, mapping: &Mapping) {
    validate_original(runtime, binding, mapping).unwrap();
    let started = std::time::Instant::now();
    loop {
        let report = runtime.poll(PollFault::None);
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        for observation in report.observations {
            runtime.deliver_observation(&observation).unwrap();
        }
        if runtime.scope().closed {
            return;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "original native scope stayed open after cleanup request; root_live={}",
            runtime.native().birth().is_live().unwrap()
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
fn actual_closed(runtime: &mut Runtime, binding: StageBinding, store: &mut NativeStore, mapping: &Mapping) {
    wait_native_closed(runtime, binding, mapping);
    // SQL records the completed observation; it is not the cleanup mechanism.
    validate_original(runtime, binding, mapping).unwrap();
    store
        .observe_resource(mapping.sql, &mapping.request.capability, ResourceReport::Closed)
        .unwrap();
}
fn persist_result_original(
    runtime: &mut Runtime,
    binding: StageBinding,
    store: &mut NativeStore,
    mapping: &Mapping,
    body: &[u8],
) -> Result<(), LiveError> {
    validate_original(runtime, binding, mapping)?;
    match store.observe_result(mapping.sql, &mapping.request.capability, [42; 16], body) {
        Ok(()) => Ok(()),
        Err(error) => Err(storage_failed(runtime, binding, error)),
    }
}
fn actual_dummy_result(runtime: &Runtime, mapping: &Mapping) -> Vec<u8> {
    let actual = std::fs::read(runtime.native().root().join("result-observed.json")).unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&actual).unwrap();
    assert_eq!(parsed["value"], 42);
    assert_eq!(parsed["exit_ok"], true);
    assert_eq!(
        runtime.stage(mapping.key).unwrap().result,
        kernel::Retention::Retained(kernel::ResultId(42))
    );
    actual
}
fn capture(runtime: &mut Runtime, binding: StageBinding, mapping: &Mapping, kind: ObservedKind) {
    validate_original(runtime, binding, mapping).unwrap();
    let mut found = false;
    fixtures::wait(|| {
        let report = runtime.poll(PollFault::None);
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        for observation in report.observations {
            if observation.kind() == kind {
                runtime.deliver_observation(&observation).unwrap();
                found = true;
            }
        }
        found
    });
}
fn open_stores(fixture: &Fixture) -> (CallerJournal, NativeStore) {
    (
        CallerJournal::create_new(&fixture.root.join("caller.sqlite3")).unwrap(),
        NativeStore::create_new(&fixture.root.join("native.sqlite3")).unwrap(),
    )
}
#[test]
fn committed_permission_with_dropped_go_reopens_without_creating_a_receiver() {
    let fixture = Fixture::new();
    let (mut journal, mut store) = open_stores(&fixture);
    let mapping = establish(&fixture, &mut journal, &mut store);
    let mut runtime = Runtime::new(mapping.scope);
    let original = runtime.prepare(&fixture, mapping.key, &mapping.request.intent).unwrap();
    store
        .observe_resource(mapping.sql, &mapping.request.capability, ResourceReport::Open)
        .unwrap();
    // A second SQL record has NO second native root. Mixing its mapping with
    // the original live binding must change neither gate and send no GO.
    let wrong_key = NativeKey {
        nonce: uuid::Uuid::new_v4().into_bytes(),
        stage: 2,
        ..mapping.request.key
    };
    let wrong_request = journal
        .bind(
            CallerKey {
                job: b"other original job".to_vec(),
                stage: 2,
                revision: 1,
            },
            b"other input",
            wrong_key,
            Capability([9; 32]),
        )
        .unwrap();
    store.create(&wrong_request).unwrap();
    let wrong_scope = Scope {
        id: uuid::Uuid::new_v4().into_bytes(),
        generation: 0,
        owner: uuid::Uuid::new_v4().into_bytes(),
    };
    let wrong_sql = store
        .bind_scope(wrong_key, &wrong_request.capability, wrong_scope)
        .unwrap();
    let wrong = Mapping {
        key: StageKey::new(
            AttemptId::new(
                u128::from_be_bytes(wrong_key.store),
                wrong_key.epoch,
                u128::from_be_bytes(wrong_key.nonce),
            ),
            wrong_key.stage,
        ),
        scope: ScopeBinding::new(
            u128::from_be_bytes(wrong_scope.id),
            wrong_scope.generation,
            u128::from_be_bytes(wrong_scope.owner),
        ),
        sql: wrong_sql,
        request: wrong_request,
    };
    let original_before = runtime.stage(mapping.key);
    let sql_before = store.get(mapping.request.key).unwrap();
    let wrong_before = store.get(wrong_key).unwrap();
    let mixed = start_original(&mut runtime, original, &mut store, &wrong, GoFault::None);
    if matches!(mixed, Ok(Dispatch::Sent | Dispatch::Unconfirmed)) {
        fixtures::wait(|| runtime.native().target_alive());
        eprintln!(
            "mixed mapping actually launched target: starts={}, SQL-A={:?}, SQL-B={:?}",
            fixture.leaf_starts(mapping.key).len(),
            store.get(wrong_key).unwrap().unwrap().gate,
            store.get(mapping.request.key).unwrap().unwrap().gate
        );
    }
    assert!(mixed.is_err(), "mixed SQL/native identities sent GO: {mixed:?}");
    assert_eq!(runtime.stage(mapping.key), original_before);
    assert_eq!(store.get(mapping.request.key).unwrap(), sql_before);
    assert_eq!(store.get(wrong_key).unwrap(), wrong_before);
    assert_eq!(fixture.root_starts(), 1);
    assert!(fixture.leaf_starts(mapping.key).is_empty());
    assert_eq!(
        start_original(&mut runtime, original, &mut store, &mapping, GoFault::BeforeWrite).unwrap(),
        Dispatch::NotSent
    );
    assert_eq!(store.get(mapping.request.key).unwrap().unwrap().gate, Gate::Consumed);
    drop(store);
    drop(journal);
    let journal = CallerJournal::open(&fixture.root.join("caller.sqlite3")).unwrap();
    let saved = journal.get(&mapping.request.caller).unwrap().unwrap();
    assert_eq!(saved, mapping.request);
    let mut store = NativeStore::open(&fixture.root.join("native.sqlite3")).unwrap();
    assert!(matches!(
        store.consume_launch(mapping.sql, &saved.capability).unwrap(),
        Consume::AlreadyConsumed
    ));
    assert_eq!(
        start_original(&mut runtime, original, &mut store, &mapping, GoFault::None).unwrap(),
        Dispatch::AlreadyTaken
    );
    actual_closed(&mut runtime, original, &mut store, &mapping);
    assert!(runtime.native().root().join("receiver-no-go").exists());
    assert_eq!(fixture.root_starts(), 1);
    assert!(fixture.leaf_starts(mapping.key).is_empty());
    let row = store.get(saved.key).unwrap().unwrap();
    assert_eq!(row.gate, Gate::Consumed);
    assert_eq!(row.spawn, None);
    assert_eq!(row.result, ResultKnowledge::Pending);
    fixture.assert_no_safety_exit();
}
#[test]
fn committed_go_with_lost_reply_reconciles_same_live_child_after_database_reopen() {
    let fixture = Fixture::new();
    let (mut journal, mut store) = open_stores(&fixture);
    let mapping = establish(&fixture, &mut journal, &mut store);
    let mut runtime = Runtime::new(mapping.scope);
    let original = runtime.prepare(&fixture, mapping.key, &mapping.request.intent).unwrap();
    store
        .observe_resource(mapping.sql, &mapping.request.capability, ResourceReport::Open)
        .unwrap();
    assert_eq!(
        start_original(&mut runtime, original, &mut store, &mapping, GoFault::AfterWrite).unwrap(),
        Dispatch::Unconfirmed
    );
    fixtures::wait(|| runtime.native().target_alive());
    let birth = runtime.native().birth().clone();
    let contender = Contender::start(runtime.native());
    drop(store);
    drop(journal); // SAME actual Runtime, Child and guardian stay alive.
    let mut journal = CallerJournal::open(&fixture.root.join("caller.sqlite3")).unwrap();
    assert_eq!(
        journal.get(&mapping.request.caller).unwrap(),
        Some(mapping.request.clone())
    );
    let mut store = NativeStore::open(&fixture.root.join("native.sqlite3")).unwrap();
    assert!(matches!(
        store.consume_launch(mapping.sql, &mapping.request.capability).unwrap(),
        Consume::AlreadyConsumed
    ));
    assert_eq!(
        start_original(&mut runtime, original, &mut store, &mapping, GoFault::None).unwrap(),
        Dispatch::AlreadyTaken
    );
    assert_eq!(runtime.native().birth(), &birth);
    assert_eq!(fixture.root_starts(), 1);
    assert_eq!(fixture.leaf_starts(mapping.key).len(), 1);
    capture(&mut runtime, original, &mapping, ObservedKind::Spawn);
    validate_original(&runtime, original, &mapping).unwrap();
    store
        .observe_spawn(mapping.sql, &mapping.request.capability, store::Spawn::Spawned)
        .unwrap();
    runtime.native().finish_target();
    capture(&mut runtime, original, &mapping, ObservedKind::Result);
    validate_original(&runtime, original, &mapping).unwrap();
    let actual = actual_dummy_result(&runtime, &mapping);
    persist_result_original(&mut runtime, original, &mut store, &mapping, &actual).unwrap();
    let result = store.get(mapping.request.key).unwrap().unwrap();
    assert!(matches!(result.result,ResultKnowledge::Retained{body,..} if body==actual));
    assert_eq!(result.resource_report, ResourceReport::Open);
    assert!(!runtime.scope().closed);
    let scope_before_ack = runtime.scope();
    let accepted = result_client::accept_then_ack(&mut journal, &mut store, &mapping.request, mapping.sql).unwrap();
    assert_eq!(accepted.body(), actual.as_slice());
    assert_eq!(accepted.binding(), mapping.sql);
    runtime.ack(original, kernel::ResultId(42)).unwrap();
    store
        .expire(mapping.sql, &mapping.request.capability, [42; 16])
        .unwrap();
    runtime.expire(original, kernel::ResultId(42)).unwrap();
    drop(journal);
    let mut journal = CallerJournal::open(&fixture.root.join("caller.sqlite3")).unwrap();
    assert_eq!(
        journal.saved_result(&mapping.request.caller).unwrap(),
        Some(accepted.clone())
    );
    assert_eq!(
        result_client::accept_then_ack(&mut journal, &mut store, &mapping.request, mapping.sql).unwrap(),
        accepted
    );
    assert!(matches!(
        store.get(mapping.request.key).unwrap().unwrap().result,
        ResultKnowledge::Expired { .. }
    ));
    assert_eq!(runtime.scope(), scope_before_ack);
    assert!(runtime.native().birth().is_live().unwrap());
    stop_original(&mut runtime, original);
    actual_closed(&mut runtime, original, &mut store, &mapping);
    let observed = contender.finish();
    assert_eq!(observed["group_present"], false);
    assert_eq!(observed["payload_after_marker"], 0);
    assert_eq!(fixture.root_starts(), 1);
    assert_eq!(fixture.leaf_starts(mapping.key).len(), 1);
    fixture.assert_no_safety_exit();
}

const UPDATE_FAILURE: &str = "fixture rejects stage update";
fn reject_stage_updates(fixture: &Fixture) {
    let checked =
        connection::CheckedConnection::open_existing(&fixture.root.join("native.sqlite3"), store::native_schema_valid)
            .unwrap();
    // Persistent, committed separately: rollback of the tested UPDATE cannot
    // remove this trigger. It blocks the final cleanup-report UPDATE too.
    checked.connection().execute_batch(&format!("CREATE TRIGGER main.fixture_reject_stage_updates BEFORE UPDATE ON stages BEGIN SELECT RAISE(ABORT,'{UPDATE_FAILURE}'); END;")).unwrap();
}
fn assert_update_rejected(error: &store::Problem) {
    match error {
        store::Problem::Sql(rusqlite::Error::SqliteFailure(code, Some(message))) => {
            assert_eq!(code.code, rusqlite::ErrorCode::ConstraintViolation);
            assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER);
            assert_eq!(message, UPDATE_FAILURE);
        }
        other @ (store::Problem::Sql(_) | store::Problem::Connection(_) | store::Problem::Rejected(_)) => {
            panic!("expected actual SQLite trigger rejection, got {other:?}")
        }
    }
}
fn assert_storage_update_rejected(error: &LiveError) {
    match error {
        LiveError::Storage(error) => assert_update_rejected(error),
        LiveError::IdentityMismatch | LiveError::Authorization(_) => {
            panic!("expected SQL operation failure, got {error:?}")
        }
    }
}
fn assert_failed_writes_after_cleanup(
    fixture: &Fixture,
    runtime: &Runtime,
    store: &mut NativeStore,
    mapping: &Mapping,
    before: &store::Snapshot,
) {
    assert!(runtime.scope().closed);
    assert!(!runtime.native().birth().is_live().unwrap());
    let rejected = store
        .observe_resource(mapping.sql, &mapping.request.capability, ResourceReport::Closed)
        .unwrap_err();
    assert_update_rejected(&rejected);
    let reopened = NativeStore::open(&fixture.root.join("native.sqlite3")).unwrap();
    assert_eq!(reopened.get(mapping.request.key).unwrap().as_ref(), Some(before));
    let journal = CallerJournal::open(&fixture.root.join("caller.sqlite3")).unwrap();
    assert_eq!(
        journal.get(&mapping.request.caller).unwrap(),
        Some(mapping.request.clone())
    );
}

#[test]
fn sql_write_failure_before_go_closes_original_scope_without_persisting_cleanup() {
    let fixture = Fixture::new();
    let (mut journal, mut store) = open_stores(&fixture);
    let mapping = establish(&fixture, &mut journal, &mut store);
    let mut runtime = Runtime::new(mapping.scope);
    let original = runtime.prepare(&fixture, mapping.key, &mapping.request.intent).unwrap();
    store
        .observe_resource(mapping.sql, &mapping.request.capability, ResourceReport::Open)
        .unwrap();
    let contender = Contender::start(runtime.native());
    reject_stage_updates(&fixture);
    let before = store.get(mapping.request.key).unwrap().unwrap();
    let birth = runtime.native().birth().clone();
    assert!(birth.is_live().unwrap());
    assert!(!runtime.scope().closed);
    let error = start_original(&mut runtime, original, &mut store, &mapping, GoFault::None).unwrap_err();
    assert_storage_update_rejected(&error);
    assert_eq!(runtime.stage(mapping.key).unwrap().gate, kernel::LaunchGate::Consumed);
    assert_eq!(
        start_original(&mut runtime, original, &mut store, &mapping, GoFault::None).unwrap(),
        Dispatch::AlreadyTaken
    );
    assert_eq!(fixture.root_starts(), 1);
    assert!(fixture.leaf_starts(mapping.key).is_empty());
    assert_eq!(store.get(mapping.request.key).unwrap().as_ref(), Some(&before));
    eprintln!(
        "Launch UPDATE rejected (SQLite 1811); original root live={}; target starts=0; waiting for process cleanup",
        birth.is_live().unwrap()
    );
    wait_native_closed(&mut runtime, original, &mapping);
    let observed = contender.finish();
    assert_eq!(observed["group_present"], false);
    assert_eq!(observed["payload_after_marker"], 0);
    assert_eq!(runtime.native().birth(), &birth);
    assert_eq!(fixture.root_starts(), 1);
    assert!(fixture.leaf_starts(mapping.key).is_empty());
    assert_failed_writes_after_cleanup(&fixture, &runtime, &mut store, &mapping, &before);
    assert_eq!(before.gate, Gate::Pending);
    assert_eq!(before.spawn, None);
    assert_eq!(before.result, ResultKnowledge::Pending);
    fixture.assert_no_safety_exit();
}

#[test]
fn sql_write_failure_after_result_closes_original_scope_without_persisting_cleanup() {
    let fixture = Fixture::new();
    let (mut journal, mut store) = open_stores(&fixture);
    let mapping = establish(&fixture, &mut journal, &mut store);
    let mut runtime = Runtime::new(mapping.scope);
    let original = runtime.prepare(&fixture, mapping.key, &mapping.request.intent).unwrap();
    store
        .observe_resource(mapping.sql, &mapping.request.capability, ResourceReport::Open)
        .unwrap();
    assert_eq!(
        start_original(&mut runtime, original, &mut store, &mapping, GoFault::None).unwrap(),
        Dispatch::Sent
    );
    fixtures::wait(|| runtime.native().target_alive());
    capture(&mut runtime, original, &mapping, ObservedKind::Spawn);
    store
        .observe_spawn(mapping.sql, &mapping.request.capability, store::Spawn::Spawned)
        .unwrap();
    let contender = Contender::start(runtime.native());
    runtime.native().finish_target();
    capture(&mut runtime, original, &mapping, ObservedKind::Result);
    let actual = actual_dummy_result(&runtime, &mapping);
    reject_stage_updates(&fixture);
    let before = store.get(mapping.request.key).unwrap().unwrap();
    let birth = runtime.native().birth().clone();
    assert!(birth.is_live().unwrap());
    assert!(!runtime.scope().closed);
    let error = persist_result_original(&mut runtime, original, &mut store, &mapping, &actual).unwrap_err();
    assert_storage_update_rejected(&error);
    assert_eq!(
        runtime.stage(mapping.key).unwrap().result,
        kernel::Retention::Retained(kernel::ResultId(42))
    );
    assert_eq!(
        start_original(&mut runtime, original, &mut store, &mapping, GoFault::None).unwrap(),
        Dispatch::AlreadyTaken
    );
    assert_eq!(store.get(mapping.request.key).unwrap().as_ref(), Some(&before));
    eprintln!(
        "Result UPDATE rejected (SQLite 1811); original root live={}; target starts=1; waiting for process cleanup",
        birth.is_live().unwrap()
    );
    wait_native_closed(&mut runtime, original, &mapping);
    let observed = contender.finish();
    assert_eq!(observed["group_present"], false);
    assert_eq!(observed["payload_after_marker"], 0);
    assert_eq!(runtime.native().birth(), &birth);
    assert_eq!(fixture.root_starts(), 1);
    assert_eq!(fixture.leaf_starts(mapping.key).len(), 1);
    assert_failed_writes_after_cleanup(&fixture, &runtime, &mut store, &mapping, &before);
    assert_eq!(before.gate, Gate::Consumed);
    assert_eq!(before.spawn, Some(store::Spawn::Spawned));
    assert_eq!(before.result, ResultKnowledge::Pending);
    fixture.assert_no_safety_exit();
}
