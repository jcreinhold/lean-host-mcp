use crate::fixtures::{self, Contender, Dispatch, Fixture, GoFault, NativeStage, Observation, ObservedKind};
use crate::kernel::{AttemptId, LaunchGate, Resources, ResultId, Retention, ScopeBinding, StageKey};
use crate::runtime::{PollFault, PrepareProblem, Runtime};
use std::time::{Duration, Instant};

fn key(stage: u64) -> StageKey {
    StageKey::new(AttemptId::new(1, 1, 1), stage)
}
fn scope(generation: u64) -> ScopeBinding {
    ScopeBinding::new(1, generation, 100)
}
fn started(runtime: &Runtime) {
    fixtures::wait(|| runtime.native().target_alive());
}
fn capture(runtime: &mut Runtime, kind: ObservedKind) -> Observation {
    let mut found = None;
    fixtures::wait(|| {
        let report = runtime.poll(PollFault::None);
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        found = report.observations.into_iter().find(|event| event.kind() == kind);
        found.is_some()
    });
    found.unwrap()
}
fn finish_scope(runtime: &mut Runtime) -> Vec<Observation> {
    let mut closed = Vec::new();
    fixtures::wait(|| {
        let report = runtime.poll(PollFault::None);
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        closed = report
            .observations
            .into_iter()
            .filter(|event| matches!(event.kind(), ObservedKind::StageClosed | ObservedKind::GenerationClosed))
            .collect();
        closed.len() == 2
    });
    closed
}
fn single_actual_start(fixture: &Fixture, key: StageKey) -> bool {
    let rows = fixture.leaf_starts(key);
    rows.len() == 1 && rows[0]["pid"].as_u64().is_some()
}

#[test]
fn actual_one_use_launch_and_same_prepare_retry_do_not_recreate_a_root() {
    let fixture = Fixture::new();
    let mut runtime = Runtime::new(scope(0));
    let start = Instant::now();
    let binding = runtime.prepare(&fixture, key(1), b"dummy stage").unwrap();
    assert_eq!(fixture.root_starts(), 1);
    assert!(fixture.leaf_starts(key(1)).is_empty());
    let launch = runtime.request_start(binding).unwrap().unwrap();
    assert_eq!(runtime.deliver_launch(&launch, GoFault::None), Dispatch::Sent);
    started(&runtime);
    assert_eq!(runtime.prepare(&fixture, key(1), b"dummy stage"), Ok(binding));
    assert!(runtime.request_start(binding).unwrap().is_none());
    assert_eq!(runtime.deliver_launch(&launch, GoFault::None), Dispatch::AlreadyTaken);
    assert!(single_actual_start(&fixture, key(1)));
    assert_eq!(fixture.root_starts(), 1);
    let contender = Contender::start(runtime.native());
    let stop = runtime.request_cancel(binding).unwrap().unwrap();
    assert_eq!(runtime.deliver_stop(&stop, false), Dispatch::Sent);
    for event in finish_scope(&mut runtime) {
        runtime.deliver_observation(&event).unwrap();
    }
    let observed = contender.finish();
    assert_eq!(observed["group_present"], false);
    assert_eq!(observed["payload_after_marker"], 0);
    assert_eq!(runtime.stage(key(1)).unwrap().resources, Resources::Closed);
    assert!(start.elapsed() < Duration::from_secs(3));
    fixture.assert_no_safety_exit();
}

#[test]
fn lost_launch_reply_reconciles_the_original_child_without_replay() {
    let fixture = Fixture::new();
    let mut runtime = Runtime::new(scope(0));
    let binding = runtime.prepare(&fixture, key(1), b"dummy stage").unwrap();
    let launch = runtime.request_start(binding).unwrap().unwrap();
    assert_eq!(
        runtime.deliver_launch(&launch, GoFault::AfterWrite),
        Dispatch::Unconfirmed
    );
    assert_eq!(runtime.stage(key(1)).unwrap().spawn, None);
    started(&runtime);
    assert!(runtime.request_start(binding).unwrap().is_none());
    assert_eq!(runtime.deliver_launch(&launch, GoFault::None), Dispatch::AlreadyTaken);
    let actual = capture(&mut runtime, ObservedKind::Spawn);
    runtime.deliver_observation(&actual).unwrap();
    assert!(single_actual_start(&fixture, key(1)));
    runtime.native().finish_target();
    let result = capture(&mut runtime, ObservedKind::Result);
    runtime.deliver_observation(&result).unwrap();
    assert_eq!(runtime.stage(key(1)).unwrap().result, Retention::Retained(ResultId(42)));
    assert_eq!(runtime.stage(key(1)).unwrap().resources, Resources::Open);
    assert!(runtime.replace(scope(1)).is_err());
    runtime.request_cancel(binding).unwrap();
    for event in finish_scope(&mut runtime) {
        runtime.deliver_observation(&event).unwrap();
    }
    fixture.assert_no_safety_exit();
}

#[test]
fn dropped_go_is_not_a_spawn_failure_and_never_reauthorizes() {
    let fixture = Fixture::new();
    let mut runtime = Runtime::new(scope(0));
    let binding = runtime.prepare(&fixture, key(1), b"dummy stage").unwrap();
    let launch = runtime.request_start(binding).unwrap().unwrap();
    assert_eq!(runtime.deliver_launch(&launch, GoFault::BeforeWrite), Dispatch::NotSent);
    assert_eq!(runtime.deliver_launch(&launch, GoFault::None), Dispatch::AlreadyTaken);
    assert!(runtime.request_start(binding).unwrap().is_none());
    let closed = finish_scope(&mut runtime);
    assert!(runtime.native().root().join("receiver-no-go").exists());
    assert_eq!(runtime.stage(key(1)).unwrap().spawn, None);
    assert_eq!(runtime.stage(key(1)).unwrap().gate, LaunchGate::Consumed);
    for event in closed {
        runtime.deliver_observation(&event).unwrap();
    }
    assert!(fixture.leaf_starts(key(1)).is_empty());
    assert_eq!(runtime.stage(key(1)).unwrap().result, Retention::Pending);
    fixture.assert_no_safety_exit();
}

#[test]
fn lost_stop_delivery_and_lost_closure_observation_do_not_invent_closure() {
    let fixture = Fixture::new();
    let mut runtime = Runtime::new(scope(0));
    let binding = runtime.prepare(&fixture, key(1), b"dummy stage").unwrap();
    let launch = runtime.request_start(binding).unwrap().unwrap();
    runtime.deliver_launch(&launch, GoFault::None);
    started(&runtime);
    let stop = runtime.request_cancel(binding).unwrap().unwrap();
    assert_eq!(runtime.deliver_stop(&stop, true), Dispatch::Unconfirmed);
    assert!(runtime.request_cancel(binding).unwrap().is_none());
    assert_eq!(runtime.stage(key(1)).unwrap().resources, Resources::Stopping);
    let mut lost = false;
    fixtures::wait(|| {
        let report = runtime.poll(PollFault::LoseClosureObservation);
        lost = report.problems.iter().any(|text| text.contains("injected loss"));
        lost
    });
    assert!(lost);
    assert!(!runtime.scope().closed);
    assert!(runtime.replace(scope(1)).is_err());
    for event in finish_scope(&mut runtime) {
        runtime.deliver_observation(&event).unwrap();
    }
    assert!(runtime.scope().closed);
    fixture.assert_no_safety_exit();
}

#[test]
fn genuine_old_callbacks_and_stop_packets_cannot_touch_replacement_work() {
    let fixture = Fixture::new();
    let mut runtime = Runtime::new(scope(0));
    let old = runtime.prepare(&fixture, key(1), b"dummy stage").unwrap();
    let launch = runtime.request_start(old).unwrap().unwrap();
    runtime.deliver_launch(&launch, GoFault::None);
    started(&runtime);
    let old_spawn = capture(&mut runtime, ObservedKind::Spawn);
    runtime.native().finish_target();
    let old_result = capture(&mut runtime, ObservedKind::Result);
    let old_stop = runtime.request_cancel(old).unwrap().unwrap();
    runtime.deliver_stop(&old_stop, false);
    let old_closed = finish_scope(&mut runtime);
    let generation = old_closed
        .iter()
        .find(|event| event.kind() == ObservedKind::GenerationClosed)
        .unwrap();
    runtime.deliver_observation(generation).unwrap(); // strong genuine scope closure permits replacement
    runtime.replace(scope(1)).unwrap();
    assert_eq!(runtime.prepare(&fixture, key(1), b"dummy stage"), Ok(old));
    assert_eq!(fixture.root_starts(), 1);
    let current = runtime.prepare(&fixture, key(2), b"later dummy stage").unwrap();
    let second = runtime.request_start(current).unwrap().unwrap();
    runtime.deliver_launch(&second, GoFault::None);
    started(&runtime);
    let contender = Contender::start(runtime.native());
    let target = runtime.native().target_identity().unwrap();
    let old_delivery = runtime.deliver_stop(&old_stop, false);
    // Sample after dispatch: only subsequent writes prove the target progressed.
    let before_heartbeat = runtime.native().heartbeat();
    fixtures::wait(|| {
        assert!(target.is_live().unwrap(), "old stop killed the replacement target");
        runtime.native().heartbeat() > before_heartbeat
    });
    assert_eq!(runtime.native().target_identity(), Some(target));
    assert_eq!(old_delivery, Dispatch::Stale);
    assert_ne!(old_spawn.root_birth(), runtime.native().birth());
    runtime.native().finish_target();
    let current_result = capture(&mut runtime, ObservedKind::Result);
    runtime.deliver_observation(&current_result).unwrap();
    assert_eq!(runtime.stage(key(2)).unwrap().result, Retention::Retained(ResultId(42)));
    let before_scope = runtime.scope();
    let before_stage = runtime.stage(key(2));
    for event in [&old_spawn, &old_result].into_iter().chain(old_closed.iter()) {
        runtime.deliver_observation(event).unwrap();
        assert_eq!(runtime.scope(), before_scope);
        assert_eq!(runtime.stage(key(2)), before_stage);
    }
    runtime.ack(old, ResultId(42)).unwrap();
    runtime.expire(old, ResultId(42)).unwrap();
    assert!(runtime.request_cancel(old).unwrap().is_none());
    runtime.deliver_observation(&old_result).unwrap();
    assert_eq!(runtime.stage(key(1)).unwrap().result, Retention::Expired(ResultId(42)));
    assert_eq!(runtime.scope(), before_scope);
    assert_eq!(runtime.stage(key(2)), before_stage);
    assert_eq!(
        runtime.prepare(&fixture, key(3), b"must use fresh generation"),
        Err(PrepareProblem::OneShotGenerationAlreadyUsed)
    );
    runtime.request_cancel(current).unwrap();
    for event in finish_scope(&mut runtime) {
        runtime.deliver_observation(&event).unwrap();
    }
    let physical = contender.finish();
    assert_eq!(physical["group_present"], false);
    assert_eq!(physical["payload_after_marker"], 0);
    assert_eq!(fixture.root_starts(), 2);
    assert!(single_actual_start(&fixture, key(1)));
    assert!(single_actual_start(&fixture, key(2)));
    fixture.assert_no_safety_exit();
}

#[test]
fn wrong_retry_negative_recreates_a_second_receiver_for_the_same_stage() {
    let fixture = Fixture::new();
    let mut runtime = Runtime::new(scope(0));
    let original = runtime.prepare(&fixture, key(1), b"dummy stage").unwrap();
    let launch = runtime.request_start(original).unwrap().unwrap();
    runtime.deliver_launch(&launch, GoFault::None);
    started(&runtime);
    // Deliberately broken adapter control: bypass prepare-history and delivery
    // slot reuse, recreate an ACTUAL receiver for the SAME logical stage, and
    // replay the captured launch into that receiver. Resending into the original
    // unchanged receiver is correctly harmless and is tested above.
    let mut duplicate = NativeStage::prepare(&fixture, key(1), original, scope(0));
    duplicate.send_go(GoFault::None);
    fixtures::wait(|| duplicate.target_alive());
    let starts = fixture.leaf_starts(key(1));
    assert_eq!(starts.len(), 2);
    assert_ne!(starts[0]["pid"], starts[1]["pid"]);
    eprintln!(
        "duplicate-receiver control: actual leaf starts {}",
        serde_json::to_string(&starts).unwrap()
    );
    assert!(
        !single_actual_start(&fixture, key(1)),
        "physical start oracle must reject wrong replay"
    );
    duplicate.request_stop().unwrap();
    fixtures::wait(|| !duplicate.closed_observations().unwrap().is_empty());
    runtime.request_cancel(original).unwrap();
    for event in finish_scope(&mut runtime) {
        runtime.deliver_observation(&event).unwrap();
    }
    fixture.assert_no_safety_exit();
}
