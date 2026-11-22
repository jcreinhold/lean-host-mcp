//! Actual service loss; new observer has durable IDs, never the lost handles.
use super::*;
use lean_toolchain::{native_guardian::NativeProcessIdentity, process_group::group_present};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::Duration,
};

const BASE_ENV: &str = "LEAN_STAGE_SERVICE_BASE";
const CASE_ENV: &str = "LEAN_STAGE_SERVICE_CUT";
#[derive(Clone, Copy)]
enum Cut {
    ConsumedBeforeGo,
    WitnessedStart,
}
impl Cut {
    fn name(self) -> &'static str {
        match self {
            Self::ConsumedBeforeGo => "consumed-before-go",
            Self::WitnessedStart => "witnessed-start",
        }
    }
    fn starts(self) -> usize {
        match self {
            Self::ConsumedBeforeGo => 0,
            Self::WitnessedStart => 1,
        }
    }
}
fn write_json(root: &Path, name: &str, value: &Value) {
    let temporary = root.join(format!("{name}.pending"));
    fs::write(&temporary, serde_json::to_vec(value).unwrap()).unwrap();
    fs::rename(temporary, root.join(name)).unwrap();
}
fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn command(fixture: &Fixture, helper: &str, cut: Cut) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", helper, "--nocapture"])
        .env(BASE_ENV, &fixture.root)
        .env(CASE_ENV, cut.name())
        .stdin(Stdio::null())
        .stdout(File::create(fixture.root.join(format!("{helper}.stdout"))).unwrap())
        .stderr(File::create(fixture.root.join(format!("{helper}.stderr"))).unwrap());
    command
}
// This is authority over only an actual service or observer Child. No original
// worker handle, custody FD or guardian channel exists in the outer controller.
struct TestChild(Child);
impl TestChild {
    fn wait(&mut self) -> ExitStatus {
        let mut status = None;
        fixtures::wait(|| {
            status = self.0.try_wait().unwrap();
            status.is_some()
        });
        status.unwrap()
    }
}
impl Drop for TestChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _kill = self.0.kill();
            let _wait = self.0.wait();
        }
    }
}
fn original_request(journal: &CallerJournal) -> SavedRequest {
    journal
        .get(&CallerKey {
            job: b"coupled original job".to_vec(),
            stage: 1,
            revision: 1,
        })
        .unwrap()
        .unwrap()
}
fn stage_key(request: &SavedRequest) -> StageKey {
    StageKey::new(
        AttemptId::new(
            u128::from_be_bytes(request.key.store),
            request.key.epoch,
            u128::from_be_bytes(request.key.nonce),
        ),
        request.key.stage,
    )
}

#[test]
fn service_helper() {
    let Some(base) = std::env::var_os(BASE_ENV).map(PathBuf::from) else {
        return;
    };
    let fixture = Fixture::open_in_helper(&base);
    let cut = std::env::var(CASE_ENV).unwrap();
    assert!(matches!(cut.as_str(), "consumed-before-go" | "witnessed-start"));
    let mut journal = CallerJournal::create_new(&fixture.root.join("caller.sqlite3")).unwrap();
    let mut store = NativeStore::create_new(&fixture.root.join("native.sqlite3")).unwrap();
    let mapping = establish(&fixture, &mut journal, &mut store);
    let mut runtime = Runtime::new(mapping.scope);
    let binding = runtime.prepare(&fixture, mapping.key, &mapping.request.intent).unwrap();
    validate_original(&runtime, binding, &mapping).unwrap();
    store
        .observe_resource(mapping.sql, &mapping.request.capability, ResourceReport::Open)
        .unwrap();
    let packet = runtime.request_start(binding).unwrap().unwrap();
    let Consume::Granted(permit) = store.consume_launch(mapping.sql, &mapping.request.capability).unwrap() else {
        panic!("fresh original consumption did not grant permission");
    };
    assert_eq!(permit.into_binding(), mapping.sql);
    if cut == "witnessed-start" {
        assert_eq!(
            runtime.deliver_launch(&packet, GoFault::AfterWrite),
            Dispatch::Unconfirmed
        );
        // Actual target marker/bytes, not merely a successful pipe write. Never
        // poll or persist the resulting spawn observation before the fault.
        fixtures::wait(|| {
            runtime.native().root().join("body-started.json").exists() && runtime.native().heartbeat() > 0
        });
        assert_eq!(fixture.leaf_starts(mapping.key).len(), 1);
    } else {
        // NativeStage still holds its GO sender. BeforeWrite would drop it and
        // cause EOF cleanup BEFORE service death, which is a different fault.
        assert!(fixture.leaf_starts(mapping.key).is_empty());
    }
    let snapshot = store.get(mapping.request.key).unwrap().unwrap();
    assert_eq!(snapshot.gate, Gate::Consumed);
    assert_eq!(snapshot.spawn, None);
    assert_eq!(snapshot.result, ResultKnowledge::Pending);
    let guardian = read_json(&runtime.native().root().join("guardian-birth.json"));
    write_json(
        &fixture.root,
        "service-ready.json",
        &json!({
            "service_pid":std::process::id(),"cut":cut,"worker":runtime.native().birth(),
            "scope_root":runtime.native().root(),"guardian":guardian
        }),
    );
    // The actual service is SIGKILLed while retaining Runtime, the worker Child,
    // GO sender (before GO) and original guardian channel. Drop cannot run.
    loop {
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn observer_helper() {
    let Some(base) = std::env::var_os(BASE_ENV).map(PathBuf::from) else {
        return;
    };
    let fixture = Fixture::open_in_helper(&base);
    let journal = CallerJournal::open(&fixture.root.join("caller.sqlite3")).unwrap();
    let mut store = NativeStore::open(&fixture.root.join("native.sqlite3")).unwrap();
    let request = original_request(&journal);
    let before = store.get(request.key).unwrap().unwrap();
    let original = Binding {
        key: request.key,
        scope: before.scope.unwrap(),
    };
    assert!(matches!(
        store.consume_launch(original, &request.capability).unwrap(),
        Consume::AlreadyConsumed
    ));
    // No native factory call, reconstructed Runtime or closure event follows a
    // missing result. IDs/scope from SQL grant no process or signal authority.
    assert_eq!(store.get(request.key).unwrap().unwrap(), before);
    assert_eq!(before.gate, Gate::Consumed);
    assert_eq!(before.spawn, None);
    assert_eq!(before.result, ResultKnowledge::Pending);
    assert_eq!(before.resource_report, ResourceReport::Open);
    assert!(!before.stop_requested);
    assert!(journal.saved_result(&request.caller).unwrap().is_none());
    write_json(
        &fixture.root,
        "observer-result.json",
        &json!({
            "observer_pid":std::process::id(),"original_identity_unchanged":true,
            "launch_still_consumed":true,"result_unknown":true,"resource_report_still_open":true
        }),
    );
}

fn service_death(cut: Cut) {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("service-death-case"), cut.name()).unwrap();
    let mut service = TestChild(command(&fixture, "service_loss::service_helper", cut).spawn().unwrap());
    fixtures::wait(|| fixture.root.join("service-ready.json").exists());
    let ready = read_json(&fixture.root.join("service-ready.json"));
    assert_eq!(ready["service_pid"], service.0.id());
    assert_eq!(ready["cut"], cut.name());
    let worker: NativeProcessIdentity = serde_json::from_value(ready["worker"].clone()).unwrap();
    let guardian: NativeProcessIdentity = serde_json::from_value(ready["guardian"]["birth"].clone()).unwrap();
    let scope_root = PathBuf::from(ready["scope_root"].as_str().unwrap());
    assert_eq!(ready["guardian"]["parent_pid"], worker.pid);
    assert_eq!(guardian.session, worker.session);
    assert_ne!(guardian.process_group, worker.process_group);
    assert!(worker.is_live().unwrap() && guardian.is_live().unwrap());
    let roots = fs::read_to_string(fixture.root.join("root-starts.jsonl")).unwrap();
    let original_root: Value = serde_json::from_str(roots.lines().next().unwrap()).unwrap();
    assert_eq!(original_root["pid"], worker.pid);
    assert_eq!(original_root["parent_pid"], service.0.id());
    let contender = Contender::start_at(&scope_root, &worker);
    assert!(scope_root.join("contender-blocked").exists());
    assert!(service.0.try_wait().unwrap().is_none());
    service.0.kill().unwrap();
    assert_eq!(service.wait().signal(), Some(9));
    // Observation only. The surviving healthy guardian supplies cleanup; this
    // process never signals/reaps an old worker or recreates its missing lease.
    fixtures::wait(|| scope_root.join("contender-acquired.json").exists());
    let acquired = read_json(&scope_root.join("contender-acquired.json"));
    assert_eq!(acquired["group_present"], false, "{acquired}");
    assert_eq!(acquired["worker_live"], false, "{acquired}");
    fixtures::wait(|| !group_present(worker.process_group).unwrap() && !guardian.is_live().unwrap());
    assert!(scope_root.join("guardian-returned").exists());
    assert!(!scope_root.join("guardian-error").exists());
    let complete = contender.finish(); // Drops its EX before the fresh observer.
    assert_eq!(complete["readiness_after_closure"], true, "{complete}");
    assert_eq!(complete["payload_after_marker"], 0, "{complete}");
    let mut observer = TestChild(command(&fixture, "service_loss::observer_helper", cut).spawn().unwrap());
    assert!(
        observer.wait().success(),
        "fresh observer failed: {}",
        fixture.root.display()
    );
    let report = read_json(&fixture.root.join("observer-result.json"));
    assert_ne!(report["observer_pid"], ready["service_pid"]);
    let journal = CallerJournal::open(&fixture.root.join("caller.sqlite3")).unwrap();
    let request = original_request(&journal);
    // Shared audit is the replay oracle. It counts real target/root starts; a
    // negative copy must leave this exact audit intact and physically replay.
    assert_eq!(
        fixture.leaf_starts(stage_key(&request)).len(),
        cut.starts(),
        "observer replayed the consumed target"
    );
    assert_eq!(fixture.root_starts(), 1, "observer started a replacement root");
    let snapshot = NativeStore::open(&fixture.root.join("native.sqlite3"))
        .unwrap()
        .get(request.key)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.gate, Gate::Consumed);
    assert_eq!(snapshot.spawn, None);
    assert_eq!(snapshot.result, ResultKnowledge::Pending);
    assert_eq!(snapshot.resource_report, ResourceReport::Open);
    fixture.assert_no_safety_exit();
    eprintln!(
        "service-death cut={} starts={} first_acquisition={} final={} observer={}",
        cut.name(),
        cut.starts(),
        acquired,
        complete,
        report
    );
}
#[test]
fn service_death_before_go_recovers_resources_without_reauthorizing() {
    service_death(Cut::ConsumedBeforeGo);
}
#[test]
fn service_death_after_witnessed_start_keeps_unknown_result_without_replay() {
    service_death(Cut::WitnessedStart);
}

#[test]
fn bound_drain_service_helper() {
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    let Some(base) = std::env::var_os(BASE_ENV).map(PathBuf::from) else {
        return;
    };
    let fixture = Fixture::open_in_helper(&base);
    let owner = File::create_new(base.join("owner.lock")).unwrap();
    owner.try_lock().unwrap();
    let mut journal = CallerJournal::create_new(&base.join("caller.sqlite3")).unwrap();
    let mut store = NativeStore::create_new(&base.join("native.sqlite3")).unwrap();
    let request = journal
        .bind(
            CallerKey {
                job: b"coupled original job".to_vec(),
                stage: 1,
                revision: 1,
            },
            b"disposable exact drain before GO",
            NativeKey {
                store: store.identity(),
                epoch: 1,
                nonce: [9; 16],
                stage: 1,
            },
            Capability([7; 32]),
        )
        .unwrap();
    store.create(&request).unwrap();
    let scope = Scope {
        id: [4; 16],
        generation: 1,
        owner: [5; 16],
    };
    let mut runtime = Runtime::new(ScopeBinding::new(
        u128::from_be_bytes(scope.id),
        scope.generation,
        u128::from_be_bytes(scope.owner),
    ));
    runtime.prepare(&fixture, stage_key(&request), &request.intent).unwrap();
    let native = runtime.native();
    let registry = base.join("owners");
    let metadata = registry.metadata().unwrap();
    let records: Vec<_> = fs::read_dir(&registry)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.file_name().unwrap().to_string_lossy().starts_with("worker-"))
        .collect();
    assert_eq!(records.len(), 1);
    let record_path = records[0].join("owner.json");
    let record = read_json(&record_path);
    let guardian = read_json(&native.root().join("guardian-birth.json"));
    let parent = NativeProcessIdentity::capture(std::process::id()).unwrap().unwrap();
    assert_eq!(record["guardian_drain"], "available");
    assert_eq!(record["parent"], json!(parent));
    assert_eq!(record["process"], json!(native.birth()));
    assert_eq!(record["guardian"], guardian["birth"]);
    let identity = store::recovery::RegistryIdentity {
        directory: registry.as_os_str().as_bytes().to_vec(),
        device: metadata.dev(),
        inode: metadata.ino(),
        generation: record["generation_id"].as_str().unwrap().to_owned(),
        boot: uuid::Uuid::parse_str(record["original_worker_boot"].as_str().unwrap())
            .unwrap()
            .into_bytes(),
        parent,
        worker: native.birth().clone(),
        guardian: serde_json::from_value(guardian["birth"].clone()).unwrap(),
    };
    let binding = store.bind_recovery_scope(&request, scope, &identity).unwrap();
    // Model loss after durable consumption and before sending any GO byte.
    assert!(matches!(
        store.consume_launch(binding, &request.capability).unwrap(),
        Consume::Granted(_)
    ));
    write_json(
        &base,
        "bound-recovery-ready.json",
        &json!({"root":native.root(),"record":record_path}),
    );
    fixtures::wait(|| base.join("bound-recovery-reap").exists());
    drop(runtime);
    assert!(matches!(
        store.consume_launch(binding, &request.capability).unwrap(),
        Consume::AlreadyConsumed
    ));
    drop(owner);
}

const RECOVERY_ACTION_ENV: &str = "LEAN_STAGE_RECOVERY_ACTION";

/// A fresh recovering process gets only the private fixture path and its cut.
/// The caller journal supplies the same original credentials on every open.
#[test]
fn bound_drain_recovery_helper() {
    use lean_toolchain::native_guardian::{NativeGuardianDrainBinding, request_bound_original_guardian_drain};
    use std::os::unix::ffi::OsStrExt;

    let Some(base) = std::env::var_os(BASE_ENV).map(PathBuf::from) else {
        return;
    };
    let action = std::env::var(RECOVERY_ACTION_ENV).unwrap();
    let fixture = Fixture::open_in_helper(&base);
    let journal = CallerJournal::open(&base.join("caller.sqlite3")).unwrap();
    let request = original_request(&journal);
    let (_, read) = NativeStore::read_recovery_scope(&base.join("native.sqlite3"), &request, None, None).unwrap();
    let owner = File::open(base.join("owner.lock")).unwrap();
    assert!(matches!(owner.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
    assert_eq!(fixture.root_starts(), 1);
    assert!(fixture.leaf_starts(stage_key(&request)).is_empty());

    let registry = Path::new(std::ffi::OsStr::from_bytes(&read.directory));
    let boot = uuid::Uuid::from_bytes(read.boot);
    let expected = NativeGuardianDrainBinding {
        registry_device: read.device,
        registry_inode: read.inode,
        generation: read.generation,
        boot: boot.to_string(),
        parent: read.parent,
        worker: read.worker,
        guardian: read.guardian,
    };
    let record_path = registry
        .join(format!("worker-{}", expected.generation))
        .join("owner.json");
    let record = read_json(&record_path);
    assert_eq!(record["generation_id"], expected.generation);
    assert_eq!(
        uuid::Uuid::parse_str(record["original_worker_boot"].as_str().unwrap()).unwrap(),
        boot
    );
    assert_eq!(record["parent"], json!(expected.parent));
    assert_eq!(record["process"], json!(expected.worker));
    assert_eq!(record["guardian"], json!(expected.guardian));

    match action.as_str() {
        "read" => assert_eq!(record["guardian_drain"], "available"),
        "observe-delivery" => assert_eq!(record["guardian_drain"], "requested"),
        "deliver" => {
            let stale = NativeGuardianDrainBinding {
                guardian: NativeProcessIdentity {
                    start: "different birth for the same guardian PID".to_owned(),
                    ..expected.guardian.clone()
                },
                ..expected.clone()
            };
            assert!(request_bound_original_guardian_drain(registry, &stale, Duration::from_millis(100)).is_err());
            assert_eq!(read_json(&record_path), record);
            fixtures::wait(|| {
                match request_bound_original_guardian_drain(registry, &expected, Duration::from_millis(100)) {
                    Ok(()) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
                    Err(error) => panic!("exact SDK recheck refused: {error}"),
                }
            });
            // Exit on a successful publication reply. The next process must
            // recover the persisted evidence independently of this process.
        }
        _ => panic!("invalid recovery fixture action"),
    }
}

fn exiting_recoverer(fixture: &Fixture, action: &str) -> u32 {
    let mut child = TestChild(
        command(
            fixture,
            "service_loss::bound_drain_recovery_helper",
            Cut::ConsumedBeforeGo,
        )
        .env(RECOVERY_ACTION_ENV, action)
        .spawn()
        .unwrap(),
    );
    let pid = child.0.id();
    assert!(child.wait().success(), "separate recovery process failed at {action}");
    pid
}

#[derive(Clone, Copy)]
enum RecoveryExit {
    AfterRead,
    AfterDelivery,
}

fn bound_drain_after_recovery_exit(cut: RecoveryExit) {
    let fixture = Fixture::new();
    let base = &fixture.root;
    let mut service = TestChild(
        command(
            &fixture,
            "service_loss::bound_drain_service_helper",
            Cut::ConsumedBeforeGo,
        )
        .spawn()
        .unwrap(),
    );
    let ready_path = base.join("bound-recovery-ready.json");
    fixtures::wait(|| ready_path.exists() || service.0.try_wait().unwrap().is_some());
    assert!(
        ready_path.exists(),
        "owned service did not publish an exact drain binding"
    );
    let ready = read_json(&ready_path);
    let scope_root = PathBuf::from(ready["root"].as_str().unwrap());
    let record_path = PathBuf::from(ready["record"].as_str().unwrap());
    let original_record = read_json(&record_path);
    let request = original_request(&CallerJournal::open(&base.join("caller.sqlite3")).unwrap());
    let db = base.join("native.sqlite3");
    let (_, original) = NativeStore::read_recovery_scope(&db, &request, None, None).unwrap();
    assert_eq!(original.parent.pid, service.0.id());
    let contender = Contender::start_at(&scope_root, &original.worker);
    let pid = rustix::process::Pid::from_raw(i32::try_from(service.0.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::STOP).unwrap();

    let owner = File::open(base.join("owner.lock")).unwrap();
    assert!(owner.try_lock().is_err());
    let stale = NativeProcessIdentity {
        start: "wrong parent birth".to_owned(),
        ..original.parent.clone()
    };
    assert!(matches!(
        NativeStore::read_recovery_scope(&db, &request, None, Some(&stale)),
        Err(store::Problem::Rejected(store::Reject::WrongScope))
    ));
    let (_, read) =
        NativeStore::read_recovery_scope(&db, &request, Some(&original.generation), Some(&original.parent)).unwrap();
    assert_eq!(read, original);
    assert_eq!(read_json(&record_path), original_record);
    assert!(read.worker.is_live().unwrap() && read.guardian.is_live().unwrap());
    assert_eq!(fixture.root_starts(), 1);
    assert!(fixture.leaf_starts(stage_key(&request)).is_empty());
    assert!(!scope_root.join("contender-acquired.json").exists());

    let (first, second) = match cut {
        RecoveryExit::AfterRead => {
            let reader = exiting_recoverer(&fixture, "read");
            assert_eq!(read_json(&record_path), original_record);
            assert!(original.worker.is_live().unwrap());
            assert!(!scope_root.join("contender-acquired.json").exists());
            (reader, exiting_recoverer(&fixture, "deliver"))
        }
        RecoveryExit::AfterDelivery => (
            exiting_recoverer(&fixture, "deliver"),
            exiting_recoverer(&fixture, "observe-delivery"),
        ),
    };
    assert_ne!(first, second);
    assert_ne!(first, service.0.id());
    assert_ne!(second, service.0.id());
    assert_eq!(read_json(&record_path)["guardian_drain"], "requested");
    fixtures::wait(|| !original.worker.is_live().unwrap());
    assert!(owner.try_lock().is_err());
    assert!(fixture.leaf_starts(stage_key(&request)).is_empty());
    fs::write(base.join("bound-recovery-reap"), b"allow original custodian to reap").unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::CONT).unwrap();
    assert!(service.wait().success());

    fixtures::wait(|| !group_present(original.worker.process_group).unwrap() && !original.guardian.is_live().unwrap());
    let acquired = contender.finish();
    assert_eq!(acquired["readiness_after_closure"], true);
    assert_eq!(acquired["payload_after_marker"], 0);
    assert_eq!(fixture.root_starts(), 1);
    assert!(fixture.leaf_starts(stage_key(&request)).is_empty());
    fixture.assert_no_safety_exit();
}

#[test]
fn bound_drain_reads_original_under_stopped_writer_lock() {
    bound_drain_after_recovery_exit(RecoveryExit::AfterRead);
}

#[test]
fn bound_drain_delivery_survives_recoverer_exit() {
    bound_drain_after_recovery_exit(RecoveryExit::AfterDelivery);
}
