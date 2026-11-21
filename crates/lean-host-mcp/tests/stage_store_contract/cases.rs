use crate::{connection::CheckedConnection, store::*};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    directory: Option<tempfile::TempDir>,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("lean-stage-store-tests-")
            .tempdir()
            .unwrap();
        let root = directory.path().canonicalize().unwrap();
        Self {
            directory: Some(directory),
            root,
        }
    }
    fn caller(&self) -> PathBuf {
        self.root.join("caller.sqlite3")
    }
    fn native(&self) -> PathBuf {
        self.root.join("native.sqlite3")
    }
    fn init(&self) -> (CallerJournal, NativeStore) {
        (
            CallerJournal::create_new(&self.caller()).unwrap(),
            NativeStore::create_new(&self.native()).unwrap(),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if thread::panicking()
            && let Some(d) = self.directory.take()
        {
            eprintln!("preserved failed store fixture: {}", d.keep().display());
        }
    }
}
fn caller(stage: u64) -> CallerKey {
    CallerKey {
        job: b"original job".to_vec(),
        stage,
        revision: 1,
    }
}
fn candidate(store: &NativeStore, stage: u64) -> NativeKey {
    NativeKey {
        store: store.identity(),
        epoch: 1,
        nonce: uuid::Uuid::from_u128(u128::from(stage)).into_bytes(),
        stage,
    }
}
fn scope(generation: u64) -> Scope {
    Scope {
        id: [3; 16],
        generation,
        owner: [4; 16],
    }
}
fn cap() -> Capability {
    Capability([7; 32])
}
fn saved(journal: &mut CallerJournal, store: &NativeStore, stage: u64) -> SavedRequest {
    journal
        .bind(caller(stage), b"exact canonical intent", candidate(store, stage), cap())
        .unwrap()
}
fn bound(journal: &mut CallerJournal, store: &mut NativeStore, stage: u64, generation: u64) -> (SavedRequest, Binding) {
    let saved = saved(journal, store, stage);
    store.create(&saved).unwrap();
    let binding = store
        .bind_scope(saved.key, &saved.capability, scope(generation))
        .unwrap();
    (saved, binding)
}
fn granted(out: Consume) -> bool {
    match out {
        Consume::Granted(permit) => {
            let _original = permit.into_binding();
            true
        }
        Consume::AlreadyConsumed | Consume::Sealed => false,
    }
}

#[test]
fn caller_retry_recovers_original_before_native_create() {
    let fixture = Fixture::new();
    let (mut journal, store) = fixture.init();
    let first = saved(&mut journal, &store, 1);
    drop(journal);
    let mut journal = CallerJournal::open(&fixture.caller()).unwrap();
    let different = NativeKey {
        nonce: [99; 16],
        epoch: 2,
        ..first.key
    };
    let recovered = journal
        .bind(caller(1), &first.intent, different, Capability([42; 32]))
        .unwrap();
    assert_eq!(recovered, first);
    assert!(store.get(first.key).unwrap().is_none());
    assert!(matches!(
        journal.bind(caller(1), b"changed", different, cap()),
        Err(Problem::Rejected(Reject::IntentChanged))
    ));
    assert_eq!(journal.get(&caller(1)).unwrap(), Some(first.clone()));
    let mut explicit = caller(1);
    explicit.revision = 2;
    assert_eq!(
        journal.bind(explicit, &first.intent, different, cap()).unwrap().key,
        different
    );
    let mut alias = caller(1);
    alias.revision = 3;
    assert!(matches!(
        journal.bind(alias, &first.intent, first.key, cap()),
        Err(Problem::Rejected(Reject::NativeIdentityAlreadyBound))
    ));
    assert!(!format!("{:?}", first.capability).contains("7, 7"));
}

#[test]
fn create_retry_never_resets_consumption_results_or_closed_epoch_history() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let (request, binding) = bound(&mut caller, &mut store, 1, 0);
    assert!(granted(store.consume_launch(binding, &request.capability).unwrap()));
    store
        .observe_result(binding, &request.capability, [42; 16], b"result")
        .unwrap();
    store.expire(binding, &request.capability, [42; 16]).unwrap();
    let old = store.get(request.key).unwrap().unwrap();
    assert_eq!(store.create(&request).unwrap(), old);
    assert_eq!(store.rotate_epoch(1).unwrap(), 2);
    assert_eq!(store.create(&request).unwrap(), old);
    let mut changed = request.clone();
    changed.intent.push(b'!');
    assert!(matches!(
        store.create(&changed),
        Err(Problem::Rejected(Reject::IntentChanged))
    ));
    let mut wrong = request.clone();
    wrong.capability = Capability([8; 32]);
    assert!(matches!(
        store.create(&wrong),
        Err(Problem::Rejected(Reject::CapabilityDenied))
    ));
    drop(store);
    let reopened = NativeStore::open(&fixture.native()).unwrap();
    assert_eq!(reopened.get(request.key).unwrap(), Some(old));
}

#[test]
fn launch_permission_is_never_reissued_after_lost_reply_or_reopen() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let (request, binding) = bound(&mut caller, &mut store, 1, 0);
    let permit = match store.consume_launch(binding, &request.capability).unwrap() {
        Consume::Granted(p) => p,
        other @ (Consume::AlreadyConsumed | Consume::Sealed) => panic!("{other:?}"),
    };
    assert_eq!(permit.into_binding(), binding); // Discard delivery: no payload runs.
    let duplicate = store.consume_launch(binding, &request.capability).unwrap();
    assert!(
        matches!(duplicate, Consume::AlreadyConsumed),
        "duplicate transaction returned a new launch permission: {duplicate:?}"
    );
    drop(store);
    let mut store = NativeStore::open(&fixture.native()).unwrap();
    let snapshot = store.get(request.key).unwrap().unwrap();
    assert_eq!(snapshot.gate, Gate::Consumed);
    assert_eq!(snapshot.spawn, None);
    assert_eq!(snapshot.result, ResultKnowledge::Pending);
    assert_eq!(snapshot.resource_report, ResourceReport::None);
    assert!(
        !granted(store.consume_launch(binding, &request.capability).unwrap()),
        "reopen reissued a consumed launch permit"
    );
    store
        .observe_spawn(binding, &request.capability, Spawn::Failed)
        .unwrap();
    assert!(matches!(
        store.observe_spawn(binding, &request.capability, Spawn::Spawned),
        Err(Problem::Rejected(Reject::ConflictingSpawn))
    ));
    assert!(!granted(store.consume_launch(binding, &request.capability).unwrap()));
}

#[test]
fn independent_connections_can_commit_only_one_launch_permit() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let (request, binding) = bound(&mut caller, &mut store, 1, 0);
    let barrier = Arc::new(Barrier::new(3));
    let mut joins = Vec::new();
    // Opening can fail; do it before spawning either barrier participant.
    let connections = [
        NativeStore::open(&fixture.native()).unwrap(),
        NativeStore::open(&fixture.native()).unwrap(),
    ];
    for mut db in connections {
        let barrier = Arc::clone(&barrier);
        let capability = request.capability.clone();
        joins.push(thread::spawn(move || {
            barrier.wait();
            granted(db.consume_launch(binding, &capability).unwrap())
        }));
    }
    barrier.wait();
    let permissions = joins
        .into_iter()
        .map(|join| join.join().unwrap())
        .filter(|granted| *granted)
        .count();
    assert_eq!(permissions, 1);
    assert_eq!(
        NativeStore::open(&fixture.native())
            .unwrap()
            .get(request.key)
            .unwrap()
            .unwrap()
            .gate,
        Gate::Consumed
    );
}

#[test]
fn cancellation_and_resource_reports_cannot_reopen_a_launch_gate() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let (first, b1) = bound(&mut caller, &mut store, 1, 0);
    store.cancel(b1, &first.capability).unwrap();
    assert!(matches!(
        store.consume_launch(b1, &first.capability).unwrap(),
        Consume::Sealed
    ));
    let (second, b2) = bound(&mut caller, &mut store, 2, 1);
    assert!(granted(store.consume_launch(b2, &second.capability).unwrap()));
    store.cancel(b2, &second.capability).unwrap();
    let current = store.get(second.key).unwrap().unwrap();
    assert!(current.stop_requested);
    assert_eq!(current.resource_report, ResourceReport::None);
    store
        .observe_resource(b2, &second.capability, ResourceReport::Closed)
        .unwrap();
    store
        .observe_resource(b2, &second.capability, ResourceReport::Open)
        .unwrap();
    store
        .observe_resource(b2, &second.capability, ResourceReport::Stopping)
        .unwrap();
    assert_eq!(
        store.get(second.key).unwrap().unwrap().resource_report,
        ResourceReport::Closed
    );
    assert!(!granted(store.consume_launch(b2, &second.capability).unwrap()));
    let (third, b3) = bound(&mut caller, &mut store, 3, 2);
    store
        .observe_resource(b3, &third.capability, ResourceReport::Stopping)
        .unwrap();
    assert!(matches!(
        store.consume_launch(b3, &third.capability).unwrap(),
        Consume::Sealed
    ));
}

#[test]
fn stale_scope_bindings_cannot_reset_highest_generation_or_launch_old_work() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let (old, b1) = bound(&mut caller, &mut store, 1, 4);
    let (current, b2) = bound(&mut caller, &mut store, 2, 5);
    assert_eq!(store.bind_scope(old.key, &old.capability, b1.scope).unwrap(), b1);
    assert!(matches!(
        store.consume_launch(b1, &old.capability),
        Err(Problem::Rejected(Reject::StaleGeneration))
    ));
    let third = saved(&mut caller, &store, 3);
    store.create(&third).unwrap();
    assert!(matches!(
        store.bind_scope(
            third.key,
            &third.capability,
            Scope {
                owner: [77; 16],
                ..scope(4)
            }
        ),
        Err(Problem::Rejected(Reject::StaleGeneration))
    ));
    let wrong = Binding {
        scope: Scope {
            owner: [88; 16],
            ..b2.scope
        },
        ..b2
    };
    assert!(matches!(
        store.consume_launch(wrong, &current.capability),
        Err(Problem::Rejected(Reject::WrongScope))
    ));
    assert!(granted(store.consume_launch(b2, &current.capability).unwrap()));
    assert!(matches!(
        store.bind_scope(current.key, &current.capability, scope(6)),
        Err(Problem::Rejected(Reject::ScopeAlreadyBound))
    ));
}

#[test]
fn old_callbacks_change_only_their_original_persistent_row() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let (old, b1) = bound(&mut caller, &mut store, 1, 0);
    let (current, b2) = bound(&mut caller, &mut store, 2, 1);
    store
        .observe_result(b1, &old.capability, [42; 16], b"old result")
        .unwrap();
    store
        .observe_result(b2, &current.capability, [42; 16], b"new result")
        .unwrap();
    let before = store.get(current.key).unwrap().unwrap();
    store.ack(b1, &old.capability, [42; 16]).unwrap();
    let reopened = NativeStore::open(&fixture.native())
        .unwrap()
        .get(current.key)
        .unwrap()
        .unwrap();
    assert_eq!(reopened, before, "old ACK changed replacement result after reopen");
    store.expire(b1, &old.capability, [42; 16]).unwrap();
    store
        .observe_result(b1, &old.capability, [42; 16], b"old result")
        .unwrap();
    store
        .observe_resource(b1, &old.capability, ResourceReport::Closed)
        .unwrap();
    store.cancel(b1, &old.capability).unwrap();
    assert_eq!(
        NativeStore::open(&fixture.native())
            .unwrap()
            .get(current.key)
            .unwrap()
            .unwrap(),
        before
    );
    assert!(matches!(
        store.get(old.key).unwrap().unwrap().result,
        ResultKnowledge::Expired { .. }
    ));
    assert!(matches!(
        store.observe_result(b1, &old.capability, [42; 16], b"different"),
        Err(Problem::Rejected(Reject::ConflictingResult))
    ));
    let wrong = Binding { scope: b2.scope, ..b1 };
    assert!(matches!(
        store.observe_resource(wrong, &old.capability, ResourceReport::Closed),
        Err(Problem::Rejected(Reject::WrongScope))
    ));
}

#[test]
fn result_retention_changes_neither_resource_report_nor_launch_state() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let (request, binding) = bound(&mut caller, &mut store, 1, 0);
    store
        .observe_resource(binding, &request.capability, ResourceReport::Open)
        .unwrap();
    let before = store.get(request.key).unwrap().unwrap();
    store
        .observe_result(binding, &request.capability, [42; 16], b"payload")
        .unwrap();
    store.ack(binding, &request.capability, [42; 16]).unwrap();
    store.expire(binding, &request.capability, [42; 16]).unwrap();
    store
        .observe_result(binding, &request.capability, [42; 16], b"payload")
        .unwrap();
    let after = store.get(request.key).unwrap().unwrap();
    assert_eq!(
        (after.gate, after.spawn, after.stop_requested, after.resource_report),
        (before.gate, before.spawn, before.stop_requested, before.resource_report)
    );
    assert!(matches!(after.result, ResultKnowledge::Expired { .. }));
    assert!(matches!(
        store.observe_result(binding, &request.capability, [42; 16], &vec![0; 65_537]),
        Err(Problem::Rejected(Reject::Oversized))
    ));
    assert!(matches!(
        store.ack(binding, &request.capability, [13; 16]),
        Err(Problem::Rejected(Reject::ConflictingResult))
    ));
}

#[test]
fn epoch_rotation_survives_reopen_and_missing_old_ids_cannot_create() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let existing = saved(&mut caller, &store, 1);
    store.create(&existing).unwrap();
    let missing = saved(&mut caller, &store, 2);
    assert_eq!(store.rotate_epoch(1).unwrap(), 2);
    drop(store);
    let mut store = NativeStore::open(&fixture.native()).unwrap();
    assert_eq!(store.epoch().unwrap(), 2);
    assert!(matches!(
        store.rotate_epoch(1),
        Err(Problem::Rejected(Reject::EpochChanged))
    ));
    assert!(matches!(
        store.create(&missing),
        Err(Problem::Rejected(Reject::ClosedEpoch))
    ));
    assert!(store.create(&existing).is_ok());
    assert!(store.get(missing.key).unwrap().is_none());
    let mut future = missing.clone();
    future.key.epoch = 3;
    assert!(matches!(
        store.create(&future),
        Err(Problem::Rejected(Reject::FutureEpoch))
    ));
    let mut wrong = missing.clone();
    wrong.key.store = [8; 16];
    assert!(matches!(
        store.get(wrong.key),
        Err(Problem::Rejected(Reject::WrongStore))
    ));
    assert!(matches!(
        store.create(&wrong),
        Err(Problem::Rejected(Reject::WrongStore))
    ));
    assert!(matches!(
        store.rotate_epoch(u64::MAX),
        Err(Problem::Rejected(Reject::IdentifierRange))
    ));
}

#[test]
fn opening_missing_incomplete_or_other_role_never_reinitializes() {
    let fixture = Fixture::new();
    let (_caller, store) = fixture.init();
    assert!(matches!(
        NativeStore::open(&fixture.caller()),
        Err(Problem::Rejected(Reject::WrongSchema))
    ));
    assert!(matches!(
        CallerJournal::open(&fixture.native()),
        Err(Problem::Rejected(Reject::WrongSchema))
    ));
    let missing = fixture.root.join("missing.sqlite3");
    assert!(NativeStore::open(&missing).is_err());
    assert!(!missing.exists());
    let incomplete = fixture.root.join("incomplete.sqlite3");
    fs::write(&incomplete, []).unwrap();
    assert!(matches!(
        NativeStore::open(&incomplete),
        Err(Problem::Rejected(Reject::WrongSchema))
    ));
    assert!(NativeStore::create_new(&incomplete).is_err());
    let fresh = NativeStore::create_new(&fixture.root.join("new.sqlite3")).unwrap();
    assert_ne!(fresh.identity(), store.identity());
}

struct ChildOwner(Option<Child>);
impl Drop for ChildOwner {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _kill = child.kill();
            let _wait = child.wait();
        }
    }
}
fn wait_for_cut(child: &mut ChildOwner, marker: &Path, expected: &str, root: &Path) {
    let started = Instant::now();
    while !marker.exists() {
        assert!(
            child.0.as_mut().unwrap().try_wait().unwrap().is_none(),
            "helper exited before cut: {}",
            fs::read_to_string(root.join("helper.stderr")).unwrap_or_default()
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "helper failed to reach bounded cut"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fs::read_to_string(marker).unwrap(), expected);
    assert!(child.0.as_mut().unwrap().try_wait().unwrap().is_none());
}
#[test]
fn process_cut_helper() {
    let Some(root) = std::env::var_os("LEAN_STAGE_STORE_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root).canonicalize().unwrap();
    assert!(root.starts_with(std::env::temp_dir().canonicalize().unwrap()));
    assert!(
        root.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("lean-stage-store-tests-")
    );
    let mut caller = CallerJournal::open(&root.join("caller.sqlite3")).unwrap();
    let mut store = NativeStore::open(&root.join("native.sqlite3")).unwrap();
    let op = std::env::var("LEAN_STAGE_STORE_OPERATION").unwrap();
    if op == "caller-bind" {
        saved(&mut caller, &store, 1);
        return;
    }
    let request = caller.get(&self::caller(1)).unwrap().unwrap();
    match op.as_str() {
        "native-create" => {
            store.create(&request).unwrap();
        }
        "consume" => {
            let binding = store.bind_scope(request.key, &request.capability, scope(0)).unwrap();
            store.consume_launch(binding, &request.capability).unwrap();
        }
        "result" => {
            let binding = store.bind_scope(request.key, &request.capability, scope(0)).unwrap();
            store
                .observe_result(binding, &request.capability, [42; 16], b"checked dummy output")
                .unwrap();
        }
        "accept-and-ack" => {
            let binding = Binding {
                key: request.key,
                scope: scope(0),
            };
            crate::result_client::accept_then_ack(&mut caller, &mut store, &request, binding).unwrap();
        }
        _ => panic!("unknown isolated cut"),
    }
}

#[test]
fn killed_transactions_recover_original_bindings_and_never_reissue_committed_go() {
    use std::os::unix::process::ExitStatusExt;
    for operation in ["caller-bind", "native-create", "consume", "result"] {
        for committed in [false, true] {
            for checkpointed in [false, true] {
                let fixture = Fixture::new();
                let (mut journal, mut store) = fixture.init();
                let key = candidate(&store, 1);
                if operation != "caller-bind" {
                    let request = saved(&mut journal, &store, 1);
                    if operation != "native-create" {
                        store.create(&request).unwrap();
                        let binding = store.bind_scope(request.key, &request.capability, scope(0)).unwrap();
                        if operation == "result" {
                            assert!(granted(store.consume_launch(binding, &request.capability).unwrap()));
                            store
                                .observe_resource(binding, &request.capability, ResourceReport::Open)
                                .unwrap();
                        }
                    }
                }
                if checkpointed {
                    store.checkpoint().unwrap();
                    let connection = CheckedConnection::open_existing(&fixture.caller(), caller_schema_valid).unwrap();
                    let row: (i64, i64, i64) = connection
                        .connection()
                        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                        })
                        .unwrap();
                    assert_eq!(row.0, 0);
                }
                // These parent connections stay idle, keeping WAL files present.
                // They own no active transaction, child or native process scope.
                let marker = fixture.root.join("cut-ready");
                let point = format!("{operation}:{}", if committed { "after" } else { "before" });
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", "tests::process_cut_helper", "--nocapture"])
                    .env("LEAN_STAGE_STORE_FIXTURE", &fixture.root)
                    .env("LEAN_STAGE_STORE_OPERATION", operation)
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
                let recovered_caller = CallerJournal::open(&fixture.caller()).unwrap();
                let mut recovered = NativeStore::open(&fixture.native()).unwrap();
                let original = recovered_caller.get(&self::caller(1)).unwrap();
                if operation == "caller-bind" {
                    assert_eq!(original.is_some(), committed);
                    if let Some(saved) = original {
                        assert_eq!(saved.key, key);
                        assert_eq!(saved.intent, b"exact canonical intent");
                        assert_eq!(saved.caller, self::caller(1));
                        assert_eq!(saved.capability, cap());
                    }
                    assert!(recovered.get(key).unwrap().is_none());
                } else {
                    let request = original.unwrap();
                    assert_eq!(request.key, key);
                    let before = recovered.get(key).unwrap();
                    match operation {
                        "native-create" => assert_eq!(before.is_some(), committed),
                        "consume" => {
                            let binding = Binding { key, scope: scope(0) };
                            let snapshot = before.unwrap();
                            assert_eq!(snapshot.gate, if committed { Gate::Consumed } else { Gate::Pending });
                            assert_eq!(snapshot.spawn, None);
                            assert_eq!(snapshot.result, ResultKnowledge::Pending);
                            let retry = recovered.consume_launch(binding, &request.capability).unwrap();
                            assert_eq!(granted(retry), !committed, "lost commit reply renewed permission");
                        }
                        "result" => {
                            let snapshot = before.unwrap();
                            assert_eq!(snapshot.gate, Gate::Consumed);
                            assert_eq!(snapshot.resource_report, ResourceReport::Open);
                            let expected = if committed {
                                use sha2::Digest;
                                ResultKnowledge::Retained {
                                    id: [42; 16],
                                    hash: sha2::Sha256::digest(b"checked dummy output").to_vec(),
                                    body: b"checked dummy output".to_vec(),
                                }
                            } else {
                                ResultKnowledge::Pending
                            };
                            assert_eq!(snapshot.result, expected);
                        }
                        _ => panic!("unknown fixture operation"),
                    }
                    // CREATE is retried only as original identity, never as payload.
                    assert_eq!(recovered.create(&request).unwrap().key, key);
                }
                // Checkpoint recovered history, close every connection, cold-open.
                recovered.checkpoint().unwrap();
                drop(recovered);
                drop(recovered_caller);
                drop(store);
                drop(journal);
                let cold_caller = CallerJournal::open(&fixture.caller()).unwrap();
                let cold_original = cold_caller.get(&self::caller(1)).unwrap();
                assert_eq!(cold_original.is_some(), operation != "caller-bind" || committed);
                if let Some(saved) = cold_original {
                    assert_eq!(saved.key, key);
                    assert_eq!(saved.capability, cap());
                }
                let cold = NativeStore::open(&fixture.native()).unwrap();
                if operation == "consume" || operation == "result" {
                    assert_eq!(cold.get(key).unwrap().unwrap().gate, Gate::Consumed);
                }
                eprintln!(
                    "process cut passed: {point}, baseline_checkpointed={checkpointed}, store={}",
                    uuid::Uuid::from_bytes(key.store)
                );
            }
        }
    }
}

#[test]
fn checked_writer_and_checkpoint_connections_preserve_all_commits() {
    let fixture = Fixture::new();
    let (mut caller, store) = fixture.init();
    let connection = CheckedConnection::open_existing(&fixture.native(), native_schema_valid).unwrap();
    assert_eq!(connection.provenance().version_number, 3_054_001);
    let barrier = Arc::new(Barrier::new(2));
    let go = Arc::clone(&barrier);
    let checkpoint = thread::spawn(move || {
        // One startup barrier only. A later SQLite error must fail the join,
        // not strand the writer at a second barrier.
        go.wait();
        for _ in 0..128 {
            let status: (i64, i64, i64) = connection
                .connection()
                .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .unwrap();
            assert!(status.0 == 0 || status.0 == 1);
            thread::sleep(Duration::from_millis(1));
        }
    });
    let mut store = store;
    barrier.wait();
    let mut keys = Vec::new();
    for stage in 1..=32 {
        let (request, binding) = bound(&mut caller, &mut store, stage, stage);
        assert!(granted(store.consume_launch(binding, &request.capability).unwrap()));
        store
            .observe_result(binding, &request.capability, [42; 16], b"committed")
            .unwrap();
        keys.push(request.key);
    }
    checkpoint.join().unwrap();
    let checked = CheckedConnection::open_existing(&fixture.native(), native_schema_valid).unwrap();
    let integrity: String = checked
        .connection()
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    let fresh = NativeStore::open(&fixture.native()).unwrap();
    for key in keys {
        let row = fresh.get(key).unwrap().unwrap();
        assert_eq!(row.gate, Gate::Consumed);
        assert!(matches!(row.result,ResultKnowledge::Retained{body,..} if body==b"committed"));
    }
}

#[test]
fn rejected_delete_mode_databases_keep_their_schema_and_journal_mode() {
    use rusqlite::{Connection, OpenFlags};
    fn inspect(path: &Path) -> (String, Vec<(String, String)>, Vec<u8>) {
        let db = Connection::open_with_flags_and_vfs(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            "unix",
        )
        .unwrap();
        let mode: String = db.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        let schema = db
            .prepare("SELECT name,sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY name")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        (mode, schema, fs::read(path).unwrap())
    }
    let fixture = Fixture::new();
    // Clean DELETE-mode files only: no hot journal or externally active WAL.
    // Query-only validation does not claim arbitrary SQLite recovery never writes.
    for (name, role, version) in [
        ("wrong-role", 1234, 1),
        ("incomplete-native", 0x4c53_4e53, 1),
        ("unsupported-native", 0x4c53_4e53, 9),
        ("incomplete-caller", 0x4c53_434a, 2),
        ("old-caller", 0x4c53_434a, 1),
        ("unsupported-caller", 0x4c53_434a, 9),
    ] {
        let path = fixture.root.join(format!("{name}.sqlite3"));
        let db = Connection::open_with_flags_and_vfs(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            "unix",
        )
        .unwrap();
        db.execute_batch(&format!("PRAGMA journal_mode=DELETE; PRAGMA application_id={role}; PRAGMA user_version={version}; CREATE TABLE keep_me(value BLOB NOT NULL); INSERT INTO keep_me VALUES(x'010203');")).unwrap();
        drop(db);
        let before = inspect(&path);
        assert_eq!(before.0, "delete");
        assert!(NativeStore::open(&path).is_err());
        let after = inspect(&path);
        assert_eq!(
            after.0, before.0,
            "native refusal changed clean DELETE database: {name}"
        );
        assert_eq!(after.1, before.1);
        assert!(after.2 == before.2, "native refusal changed database bytes");
        assert!(CallerJournal::open(&path).is_err());
        let after = inspect(&path);
        assert_eq!(
            after.0, before.0,
            "caller refusal changed clean DELETE database: {name}"
        );
        assert_eq!(after.1, before.1);
        assert!(after.2 == before.2, "caller refusal changed database bytes");
    }
    let empty = fixture.root.join("incomplete-empty.sqlite3");
    fs::write(&empty, []).unwrap();
    assert!(NativeStore::open(&empty).is_err());
    assert_eq!(fs::read(&empty).unwrap(), Vec::<u8>::new());
    assert!(CallerJournal::open(&empty).is_err());
    assert!(fs::read(&empty).unwrap().is_empty());
}

#[path = "caller_results.rs"]
mod caller_results;

#[path = "boundary_cases.rs"]
mod boundary_cases;
