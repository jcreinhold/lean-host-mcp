//! Private client byte boundary; no OS process or service listener is created.
use super::*;
use crate::store::boundary::{ClientReply, DecodeError, decode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn key_json(key: NativeKey) -> Value {
    json!({"store":key.store,"epoch":key.epoch,"nonce":key.nonce,"stage":key.stage})
}
fn command(op: &str, request: &SavedRequest) -> Value {
    json!({"version":1,"command":{"op":op,"key":key_json(request.key),"capability":request.capability.0}})
}
#[allow(
    clippy::unwrap_in_result,
    reason = "fixture encoding/decoding failures must fail the test, not masquerade as a store rejection"
)]
fn call(store: &mut NativeStore, value: &Value) -> Result<ClientReply, Problem> {
    let bytes = serde_json::to_vec(value).unwrap();
    store.client(decode(&bytes).unwrap())
}
fn create(store: &mut NativeStore, request: &SavedRequest) -> ClientReply {
    let mut value = command("create", request);
    value["command"]["intent"] = json!(request.intent);
    call(store, &value).unwrap()
}
fn snapshot(reply: ClientReply) -> Snapshot {
    match reply {
        ClientReply::Snapshot(snapshot) => *snapshot,
        ClientReply::Discovery { .. } => panic!("expected original attempt snapshot"),
    }
}
fn assert_rejected(result: Result<ClientReply, Problem>, expected: &Reject) {
    assert!(matches!(result,Err(Problem::Rejected(actual)) if &actual==expected));
}
fn assert_bad_record(result: &Result<Option<Snapshot>, Problem>) {
    assert!(matches!(result, Err(Problem::Sql(rusqlite::Error::InvalidQuery))));
}
#[test]
fn client_capability_rejections_preserve_original_record() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let request = saved(&mut caller, &store, 1);
    let original = snapshot(create(&mut store, &request));
    for op in ["get", "cancel", "start", "ack"] {
        let mut value = command(op, &request);
        value["command"]["capability"] = json!(vec![8; 32]);
        if op == "ack" {
            value["command"]["result_id"] = json!(vec![42; 16]);
            value["command"]["result_digest"] = json!(vec![0; 32]);
        }
        assert_rejected(call(&mut store, &value), &Reject::CapabilityDenied);
        assert_eq!(store.get(request.key).unwrap(), Some(original.clone()));
    }
    assert_eq!(snapshot(call(&mut store, &command("get", &request)).unwrap()), original);
    let raw = CheckedConnection::open_existing(&fixture.native(), native_schema_valid).unwrap();
    assert_eq!(
        raw.connection()
            .query_row("SELECT count(*) FROM scope_heads", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[test]
fn client_create_retry_and_start_never_grant_dispatch() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let request = saved(&mut caller, &store, 1);
    let original = snapshot(create(&mut store, &request));
    assert_eq!(snapshot(create(&mut store, &request)), original);
    let mut changed = command("create", &request);
    changed["command"]["intent"] = json!(b"changed");
    assert_rejected(call(&mut store, &changed), &Reject::IntentChanged);
    changed["command"]["intent"] = json!(request.intent);
    changed["command"]["capability"] = json!(vec![9; 32]);
    changed["command"]["intent"] = json!(b"wrong token and changed guess");
    assert_rejected(call(&mut store, &changed), &Reject::CapabilityDenied);
    assert_rejected(call(&mut store, &command("start", &request)), &Reject::DispatchDisabled);
    assert_eq!(store.get(request.key).unwrap(), Some(original));
    assert!(
        matches!(call(&mut store,&json!({"version":1,"command":{"op":"discover"}})).unwrap(),ClientReply::Discovery{store:id,acceptance_epoch:1} if id==request.key.store)
    );
}
#[test]
fn pre_admission_cancel_survives_reopen_without_scope_or_closure() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let request = saved(&mut caller, &store, 1);
    create(&mut store, &request);
    let cancelled = snapshot(call(&mut store, &command("cancel", &request)).unwrap());
    assert_eq!(cancelled.gate, Gate::Sealed);
    assert_eq!(cancelled.scope, None);
    assert!(cancelled.stop_requested);
    assert_eq!(cancelled.resource_report, ResourceReport::None);
    assert_eq!(cancelled.spawn, None);
    assert_eq!(cancelled.result, ResultKnowledge::Pending);
    drop(store);
    let mut store = NativeStore::open(&fixture.native()).unwrap();
    assert_eq!(
        snapshot(call(&mut store, &command("cancel", &request)).unwrap()),
        cancelled
    );
    assert_eq!(snapshot(create(&mut store, &request)), cancelled);
    let reply = store
        .admission(command("get", &request).to_string().as_bytes())
        .unwrap();
    assert_eq!(reply["execution"], "not_started");
    assert_eq!(reply["cancel_requested"], true);
    assert!(matches!(
        store.bind_scope(request.key, &request.capability, scope(0)),
        Err(Problem::Rejected(Reject::LaunchSealed))
    ));
    let raw = CheckedConnection::open_existing(&fixture.native(), native_schema_valid).unwrap();
    assert_eq!(
        raw.connection()
            .query_row("SELECT count(*) FROM scope_heads", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(store.get(request.key).unwrap(), Some(cancelled));
}
#[test]
fn client_ack_requires_original_result_digest_and_preserves_expiry() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let (request, binding) = bound(&mut caller, &mut store, 1, 0);
    // Generic retained fact may be preparation evidence; never infer target spawn.
    store
        .observe_result(binding, &request.capability, [42; 16], b"inline observation")
        .unwrap();
    let original = snapshot(call(&mut store, &command("get", &request)).unwrap());
    assert_eq!(original.spawn, None);
    assert_eq!(original.gate, Gate::Pending);
    let mut ack = command("ack", &request);
    ack["command"]["result_id"] = json!(vec![42; 16]);
    ack["command"]["result_digest"] = json!(vec![0; 32]);
    assert_rejected(call(&mut store, &ack), &Reject::ConflictingResult);
    assert_eq!(store.get(request.key).unwrap(), Some(original.clone()));
    ack["command"]["result_digest"] = json!(Sha256::digest(b"inline observation").to_vec());
    ack["command"]["result_id"] = json!(vec![43; 16]);
    assert_rejected(call(&mut store, &ack), &Reject::ConflictingResult);
    assert_eq!(store.get(request.key).unwrap(), Some(original));
    ack["command"]["result_id"] = json!(vec![42; 16]);
    assert!(matches!(
        snapshot(call(&mut store, &ack).unwrap()).result,
        ResultKnowledge::Acknowledged { .. }
    ));
    store.expire(binding, &request.capability, [42; 16]).unwrap();
    assert!(matches!(
        snapshot(call(&mut store, &ack).unwrap()).result,
        ResultKnowledge::Expired { .. }
    ));
}
#[test]
fn client_decoder_rejects_private_ambiguous_and_oversized_messages() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let request = saved(&mut caller, &store, 1);
    let original = snapshot(create(&mut store, &request));
    for op in [
        "bind_scope",
        "consume_launch",
        "observe_spawn",
        "observe_result",
        "observe_resource",
        "rotate_epoch",
        "expire",
    ] {
        assert_eq!(
            decode(&serde_json::to_vec(&command(op, &request)).unwrap()).unwrap_err(),
            DecodeError::Malformed
        );
    }
    for payload in [
        br#"{"version":1,"command":{"op":"discover","observe_resource":"forged"}}"#.as_slice(),
        br#"{"version":1,"command":{"op":"discover","unexpected":1,"unexpected":2}}"#.as_slice(),
    ] {
        assert_eq!(decode(payload).unwrap_err(), DecodeError::Malformed);
    }
    let get = command("get", &request);
    for field in ["scope", "owner", "resources", "spawn", "result"] {
        let mut value = get.clone();
        value["command"][field] = json!("forged closed");
        assert_eq!(
            decode(&serde_json::to_vec(&value).unwrap()).unwrap_err(),
            DecodeError::Malformed
        );
    }
    let text = serde_json::to_string(&get).unwrap();
    let duplicate = text.replace("\"version\":1", "\"version\":1,\"version\":1");
    assert_eq!(decode(duplicate.as_bytes()).unwrap_err(), DecodeError::Malformed);
    let duplicate = text.replace("\"epoch\":1", "\"epoch\":1,\"epoch\":1");
    assert_eq!(decode(duplicate.as_bytes()).unwrap_err(), DecodeError::Malformed);
    let mut unsupported = get.clone();
    unsupported["version"] = json!(2);
    assert_eq!(
        decode(&serde_json::to_vec(&unsupported).unwrap()).unwrap_err(),
        DecodeError::Version
    );
    let mut short = get.clone();
    short["command"]["capability"] = json!(vec![7; 31]);
    assert_eq!(
        decode(&serde_json::to_vec(&short).unwrap()).unwrap_err(),
        DecodeError::Malformed
    );
    let mut extra = get;
    extra["command"]["key"]["scope"] = json!(vec![0; 16]);
    assert_eq!(
        decode(&serde_json::to_vec(&extra).unwrap()).unwrap_err(),
        DecodeError::Malformed
    );
    let mut large = command("create", &request);
    large["command"]["intent"] = json!(vec![0; 65_537]);
    assert_eq!(
        decode(&serde_json::to_vec(&large).unwrap()).unwrap_err(),
        DecodeError::Oversized
    );
    assert_eq!(decode(&vec![b' '; 270_000]).unwrap_err(), DecodeError::Oversized);
    assert_eq!(decode(b"{\"version\":1").unwrap_err(), DecodeError::Malformed);
    assert_eq!(
        decode(format!("{text}{text}").as_bytes()).unwrap_err(),
        DecodeError::Malformed
    );
    assert!(!format!("{:?}", decode(text.as_bytes()).unwrap()).contains("capability"));
    assert_eq!(store.get(request.key).unwrap(), Some(original));
}
#[test]
fn malformed_native_rows_fail_without_repair_or_missing_fallback() {
    for update in [
        "intent=zeroblob(65537)",
        "result_body=zeroblob(65537)",
        "result_hash=zeroblob(65537)",
        "scope=NULL",
        "owner=zeroblob(65537)",
        "stop_requested=2",
        "gate=9",
        "spawn=1",
        "result_kind=0",
        "result_kind=3",
        "result_id=NULL",
        "resource_report=9",
    ] {
        let fixture = Fixture::new();
        let (mut caller, mut store) = fixture.init();
        let (request, binding) = bound(&mut caller, &mut store, 1, 0);
        store
            .observe_result(binding, &request.capability, [42; 16], b"original result")
            .unwrap();
        let raw = CheckedConnection::open_existing(&fixture.native(), native_schema_valid).unwrap();
        raw.connection()
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        raw.connection()
            .execute_batch(&format!("UPDATE stages SET {update}"))
            .unwrap();
        let read_row = || {
            raw.connection()
                .query_row("SELECT * FROM stages", [], |row| {
                    (0..row.as_ref().column_count())
                        .map(|column| row.get::<_, rusqlite::types::Value>(column))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
        };
        let before = read_row();
        let before_version: i64 = raw
            .connection()
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .unwrap();
        assert_bad_record(&store.get(request.key));
        assert!(matches!(
            call(&mut store, &command("get", &request)),
            Err(Problem::Sql(rusqlite::Error::InvalidQuery))
        ));
        assert_eq!(
            raw.connection()
                .query_row("SELECT count(*) FROM stages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(read_row(), before);
        assert_eq!(
            raw.connection()
                .pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            before_version
        );
    }
}
#[test]
fn changed_inline_bytes_and_oversized_identity_blobs_are_rejected() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let (request, binding) = bound(&mut caller, &mut store, 1, 0);
    store
        .observe_result(binding, &request.capability, [42; 16], b"original")
        .unwrap();
    let raw = CheckedConnection::open_existing(&fixture.native(), native_schema_valid).unwrap();
    raw.connection()
        .execute("UPDATE stages SET result_body=?1", [b"changed".as_slice()])
        .unwrap();
    assert!(matches!(
        store.get(request.key),
        Err(Problem::Rejected(Reject::InvalidResult))
    ));
    raw.connection().execute_batch("PRAGMA ignore_check_constraints=ON; UPDATE stages SET result_body=x'6f726967696e616c',cap_hash=zeroblob(65537)").unwrap();
    assert!(matches!(
        call(&mut store, &command("get", &request)),
        Err(Problem::Sql(rusqlite::Error::InvalidQuery))
    ));
    raw.connection()
        .execute(
            "UPDATE stages SET cap_hash=?1",
            [Sha256::digest(request.capability.0).as_slice()],
        )
        .unwrap();
    raw.connection()
        .execute_batch("UPDATE scope_heads SET owner=zeroblob(65537)")
        .unwrap();
    assert!(matches!(
        store.consume_launch(binding, &request.capability),
        Err(Problem::Sql(rusqlite::Error::InvalidQuery))
    ));
    assert_eq!(store.get(request.key).unwrap().unwrap().gate, Gate::Pending);
    raw.connection()
        .execute_batch("UPDATE meta SET store=zeroblob(65537)")
        .unwrap();
    assert!(matches!(
        NativeStore::open(&fixture.native()),
        Err(Problem::Connection(crate::connection::ConnectionError::Sqlite(
            rusqlite::Error::InvalidQuery
        )))
    ));
}
#[test]
fn caller_reads_reject_oversized_persisted_bytes_before_owned_copy() {
    for update in [
        "UPDATE accepted_results SET body=zeroblob(65537)",
        "UPDATE accepted_results SET owner=zeroblob(65537)",
        "UPDATE bindings SET capability=zeroblob(65537)",
        "UPDATE bindings SET intent=zeroblob(65537)",
    ] {
        let fixture = Fixture::new();
        let (mut caller, mut store) = fixture.init();
        let (request, binding) = bound(&mut caller, &mut store, 1, 0);
        store
            .observe_result(binding, &request.capability, [42; 16], b"original")
            .unwrap();
        crate::result_client::accept_then_ack(&mut caller, &mut store, &request, binding).unwrap();
        let raw = CheckedConnection::open_existing(&fixture.caller(), caller_schema_valid).unwrap();
        raw.connection()
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        raw.connection().execute_batch(update).unwrap();
        if update.contains("bindings") {
            assert!(matches!(
                caller.get(&request.caller),
                Err(Problem::Sql(rusqlite::Error::InvalidQuery))
            ));
        } else {
            assert!(matches!(
                caller.saved_result(&request.caller),
                Err(Problem::Sql(rusqlite::Error::InvalidQuery))
            ));
        }
    }
}
#[test]
fn maximum_inline_create_and_result_remain_readable() {
    let fixture = Fixture::new();
    let (mut caller, mut store) = fixture.init();
    let bytes = vec![255; 65_536];
    let request = caller
        .bind(super::caller(1), &bytes, candidate(&store, 1), cap())
        .unwrap();
    assert_eq!(snapshot(create(&mut store, &request)).intent, bytes);
    let binding = store.bind_scope(request.key, &request.capability, scope(0)).unwrap();
    store
        .observe_result(binding, &request.capability, [42; 16], &bytes)
        .unwrap();
    assert!(
        matches!(snapshot(call(&mut store,&command("get",&request)).unwrap()).result,ResultKnowledge::Retained{body,..} if body==bytes)
    );
}
