#![allow(
    clippy::unwrap_used,
    reason = "regressions fail immediately when a required transition or accepted stage is absent"
)]

use super::*;

const ATTEMPT: AttemptId = AttemptId::new(1, 2, 3);
const RESULT: ResultId = ResultId(11);
fn key(stage: u64) -> StageKey {
    StageKey::new(ATTEMPT, stage)
}
fn scope(generation: u64) -> ScopeBinding {
    ScopeBinding::new(10, generation, 20)
}
fn accepted(kernel: &mut Kernel, stage: u64) -> StageBinding {
    kernel.accept(key(stage), b"immutable command").unwrap()
}
fn launch(kernel: &mut Kernel, binding: StageBinding) {
    assert_eq!(kernel.apply(binding, Event::Authorize), Ok(Effect::None));
    assert_eq!(kernel.apply(binding, Event::Go), Ok(Effect::Launch(binding)));
}
fn rejected_unchanged(kernel: &mut Kernel, binding: StageBinding, event: Event, rejected: Rejected) {
    let before = kernel.clone();
    assert_eq!(kernel.apply(binding, event), Err(rejected));
    assert_eq!(*kernel, before);
}

#[test]
fn attempt_identity_includes_store_epoch_nonce_and_stage() {
    let mut kernel = Kernel::new(scope(0));
    for attempt in [
        ATTEMPT,
        AttemptId::new(2, 2, 3),
        AttemptId::new(1, 3, 3),
        AttemptId::new(1, 2, 4),
    ] {
        let first = kernel.accept(StageKey::new(attempt, 1), b"same").unwrap();
        kernel.apply(first, Event::StageClosed).unwrap();
        let next = kernel.accept(StageKey::new(attempt, 2), b"same").unwrap();
        kernel.apply(next, Event::StageClosed).unwrap();
    }
    assert_eq!(kernel.stages.len(), 8);
}

#[test]
fn identical_accept_retry_never_reactivates_or_changes_binding() {
    let mut kernel = Kernel::new(scope(0));
    let old = accepted(&mut kernel, 1);
    kernel.apply(old, Event::StageClosed).unwrap();
    let current = accepted(&mut kernel, 2);
    let before = kernel.clone();
    assert_eq!(kernel.accept(key(1), b"immutable command"), Ok(old));
    assert_eq!(kernel, before);
    assert_eq!(kernel.accept(key(1), b"changed command"), Err(Rejected::IntentChanged));
    assert_eq!(kernel, before);
    assert_eq!(kernel.scope().active, Some(current.key));
}

#[test]
fn go_is_consumed_before_spawn_and_duplicate_authorize_never_reopens_it() {
    let mut kernel = Kernel::new(scope(0));
    let stage = accepted(&mut kernel, 1);
    rejected_unchanged(&mut kernel, stage, Event::Go, Rejected::NotAuthorized);
    launch(&mut kernel, stage);
    let before = kernel.clone();
    assert_eq!(kernel.apply(stage, Event::Go), Ok(Effect::None));
    assert_eq!(kernel.apply(stage, Event::Authorize), Ok(Effect::None));
    assert_eq!(kernel.apply(stage, Event::Go), Ok(Effect::None));
    assert_eq!(kernel, before);
    assert_eq!(kernel.stage(stage.key).unwrap().spawn, None);
}

#[test]
fn spawn_failure_does_not_retry_the_consumed_gate() {
    let mut kernel = Kernel::new(scope(0));
    let stage = accepted(&mut kernel, 1);
    launch(&mut kernel, stage);
    kernel.apply(stage, Event::SpawnObserved(SpawnFact::Failed)).unwrap();
    assert_eq!(kernel.apply(stage, Event::Go), Ok(Effect::None));
    assert_eq!(kernel.stage(stage.key).unwrap().gate, LaunchGate::Consumed);
    assert_eq!(kernel.scope().active, Some(stage.key));
    rejected_unchanged(
        &mut kernel,
        stage,
        Event::SpawnObserved(SpawnFact::Spawned),
        Rejected::ConflictingSpawn,
    );
}

#[test]
fn changed_owner_generation_or_scope_cannot_deliver_stage_facts() {
    for wrong_scope in [ScopeBinding::new(10, 0, 21), scope(1), ScopeBinding::new(11, 0, 20)] {
        for event in [
            Event::Authorize,
            Event::Go,
            Event::Cancel,
            Event::StageClosed,
            Event::ResultObserved(RESULT),
            Event::Ack(RESULT),
            Event::SpawnObserved(SpawnFact::Spawned),
        ] {
            let mut kernel = Kernel::new(scope(0));
            let stage = accepted(&mut kernel, 1);
            let wrong = StageBinding {
                scope: wrong_scope,
                ..stage
            };
            rejected_unchanged(&mut kernel, wrong, event, Rejected::BindingMismatch);
        }
    }
}

#[test]
fn unknown_stage_is_rejected_without_mutation() {
    let mut kernel = Kernel::new(scope(0));
    rejected_unchanged(
        &mut kernel,
        StageBinding {
            key: key(99),
            scope: scope(0),
        },
        Event::StageClosed,
        Rejected::UnknownStage,
    );
}

#[test]
fn cancel_before_go_seals_even_before_authorization_but_does_not_claim_closure() {
    for authorized in [false, true] {
        let mut kernel = Kernel::new(scope(0));
        let stage = accepted(&mut kernel, 1);
        if authorized {
            kernel.apply(stage, Event::Authorize).unwrap();
        }
        assert_eq!(kernel.apply(stage, Event::Cancel), Ok(Effect::RequestStop(stage)));
        let before = kernel.clone();
        assert_eq!(kernel.apply(stage, Event::Cancel), Ok(Effect::None));
        assert_eq!(kernel.apply(stage, Event::Authorize), Ok(Effect::None));
        assert_eq!(kernel.apply(stage, Event::Go), Ok(Effect::None));
        assert_eq!(kernel, before);
        assert_eq!(kernel.stage(stage.key).unwrap().resources, Resources::Stopping);
        assert_eq!(kernel.accept(key(2), b"next"), Err(Rejected::Busy));
    }
}

#[test]
fn cancel_after_go_can_race_spawn_until_a_strong_closure_observation() {
    let mut kernel = Kernel::new(scope(0));
    let stage = accepted(&mut kernel, 1);
    launch(&mut kernel, stage);
    assert_eq!(kernel.apply(stage, Event::Cancel), Ok(Effect::RequestStop(stage)));
    assert_eq!(kernel.scope().active, Some(stage.key));
    kernel.apply(stage, Event::SpawnObserved(SpawnFact::Spawned)).unwrap();
    assert_eq!(kernel.stage(stage.key).unwrap().resources, Resources::Stopping);
    assert_eq!(kernel.accept(key(2), b"next"), Err(Rejected::Busy));
    kernel.apply(stage, Event::StageClosed).unwrap();
    assert_eq!(kernel.scope().active, None);
    assert!(kernel.accept(key(2), b"next").is_ok());
}

#[test]
fn closure_before_go_seals_all_later_authorization() {
    for authorized in [false, true] {
        let mut kernel = Kernel::new(scope(0));
        let stage = accepted(&mut kernel, 1);
        if authorized {
            kernel.apply(stage, Event::Authorize).unwrap();
        }
        kernel.apply(stage, Event::StageClosed).unwrap();
        assert_eq!(kernel.stage(stage.key).unwrap().gate, LaunchGate::Sealed);
        rejected_unchanged(&mut kernel, stage, Event::Authorize, Rejected::NotCurrentStage);
        rejected_unchanged(&mut kernel, stage, Event::Go, Rejected::NotCurrentStage);
    }
}

#[test]
fn result_ack_and_expiry_do_not_release_or_reauthorize_active_work() {
    let mut kernel = Kernel::new(scope(0));
    let stage = accepted(&mut kernel, 1);
    launch(&mut kernel, stage);
    let before = kernel.scope();
    for event in [
        Event::ResultObserved(RESULT),
        Event::Ack(RESULT),
        Event::ExpireResult(RESULT),
        Event::ResultObserved(RESULT),
        Event::Ack(RESULT),
    ] {
        kernel.apply(stage, event).unwrap();
        assert_eq!(kernel.scope(), before);
        assert_eq!(kernel.stage(stage.key).unwrap().resources, Resources::Open);
        assert_eq!(kernel.apply(stage, Event::Go), Ok(Effect::None));
    }
    assert_eq!(kernel.stage(stage.key).unwrap().result, Retention::Expired(RESULT));
    assert_eq!(kernel.stage(stage.key).unwrap().gate, LaunchGate::Consumed);
}

#[test]
fn result_conflicts_and_ack_before_a_known_result_reject_unchanged() {
    let mut kernel = Kernel::new(scope(0));
    let stage = accepted(&mut kernel, 1);
    for event in [Event::Ack(RESULT), Event::ExpireResult(RESULT)] {
        rejected_unchanged(&mut kernel, stage, event, Rejected::ResultUnknown);
    }
    kernel.apply(stage, Event::ResultObserved(RESULT)).unwrap();
    for event in [
        Event::ResultObserved(ResultId(12)),
        Event::Ack(ResultId(12)),
        Event::ExpireResult(ResultId(12)),
    ] {
        rejected_unchanged(&mut kernel, stage, event, Rejected::ConflictingResult);
    }
}

#[test]
fn expiry_without_ack_preserves_the_binding_and_result_tombstone() {
    let mut kernel = Kernel::new(scope(0));
    let stage = accepted(&mut kernel, 1);
    kernel.apply(stage, Event::ResultObserved(RESULT)).unwrap();
    kernel.apply(stage, Event::ExpireResult(RESULT)).unwrap();
    kernel.apply(stage, Event::StageClosed).unwrap();
    let before = kernel.clone();
    assert_eq!(kernel.accept(stage.key, b"immutable command"), Ok(stage));
    kernel.apply(stage, Event::ResultObserved(RESULT)).unwrap();
    kernel.apply(stage, Event::Ack(RESULT)).unwrap();
    assert_eq!(kernel, before);
}

#[test]
fn matching_result_after_ack_does_not_reset_acknowledgment() {
    let mut kernel = Kernel::new(scope(0));
    let stage = accepted(&mut kernel, 1);
    kernel.apply(stage, Event::ResultObserved(RESULT)).unwrap();
    kernel.apply(stage, Event::Ack(RESULT)).unwrap();
    let before = kernel.clone();
    kernel.apply(stage, Event::ResultObserved(RESULT)).unwrap();
    assert_eq!(kernel, before);
}

#[test]
fn resource_closure_with_no_result_does_not_invent_a_result() {
    let mut kernel = Kernel::new(scope(0));
    let stage = accepted(&mut kernel, 1);
    kernel.observe_generation_closed(scope(0)).unwrap();
    assert_eq!(kernel.stage(stage.key).unwrap().result, Retention::Pending);
    assert_eq!(kernel.stage(stage.key).unwrap().resources, Resources::Closed);
    kernel.replace_generation(scope(1)).unwrap();
    assert!(kernel.accept(key(2), b"new explicit stage").is_ok());
}

#[test]
fn late_spawn_fact_updates_only_history_after_full_closure_and_replacement() {
    for fact in [SpawnFact::Spawned, SpawnFact::Failed] {
        let mut kernel = Kernel::new(scope(0));
        let old = accepted(&mut kernel, 1);
        launch(&mut kernel, old);
        kernel.observe_generation_closed(scope(0)).unwrap();
        kernel.replace_generation(scope(1)).unwrap();
        let current = accepted(&mut kernel, 2);
        launch(&mut kernel, current);
        let before_scope = kernel.scope();
        let before_stage = kernel.stage(current.key);
        kernel.apply(old, Event::SpawnObserved(fact)).unwrap();
        assert_eq!(kernel.stage(old.key).unwrap().spawn, Some(fact));
        assert_eq!(kernel.stage(old.key).unwrap().resources, Resources::Closed);
        assert_eq!(kernel.scope(), before_scope);
        assert_eq!(kernel.stage(current.key), before_stage);
        rejected_unchanged(&mut kernel, old, Event::Go, Rejected::NotCurrentStage);
    }
}

#[test]
fn unconsumed_or_sealed_gate_cannot_gain_a_spawn_fact() {
    let mut kernel = Kernel::new(scope(0));
    let stage = accepted(&mut kernel, 1);
    rejected_unchanged(
        &mut kernel,
        stage,
        Event::SpawnObserved(SpawnFact::Spawned),
        Rejected::NoConsumedGate,
    );
    kernel.apply(stage, Event::Cancel).unwrap();
    rejected_unchanged(
        &mut kernel,
        stage,
        Event::SpawnObserved(SpawnFact::Failed),
        Rejected::NoConsumedGate,
    );
}

#[test]
fn stage_cleanup_is_not_full_generation_closure() {
    let mut kernel = Kernel::new(scope(0));
    let stage = accepted(&mut kernel, 1);
    kernel.apply(stage, Event::StageClosed).unwrap();
    let before = kernel.clone();
    assert_eq!(kernel.replace_generation(scope(1)), Err(Rejected::ScopeStillOpen));
    assert_eq!(kernel, before);
}

#[test]
fn generation_closure_seals_pending_authority_without_erasing_consumption() {
    for consumed in [false, true] {
        let mut kernel = Kernel::new(scope(0));
        let stage = accepted(&mut kernel, 1);
        kernel.apply(stage, Event::Authorize).unwrap();
        if consumed {
            kernel.apply(stage, Event::Go).unwrap();
        }
        kernel.observe_generation_closed(scope(0)).unwrap();
        assert_eq!(
            kernel.stage(stage.key).unwrap().gate,
            if consumed {
                LaunchGate::Consumed
            } else {
                LaunchGate::Sealed
            }
        );
        assert_eq!(kernel.accept(key(2), b"new"), Err(Rejected::GenerationClosed));
        rejected_unchanged(&mut kernel, stage, Event::Go, Rejected::NotCurrentStage);
        let before = kernel.clone();
        kernel.observe_generation_closed(scope(0)).unwrap();
        assert_eq!(kernel, before);
    }
}

#[test]
fn historical_accept_never_rebinds_after_replacement_even_when_result_expired() {
    let mut kernel = Kernel::new(scope(0));
    let old = accepted(&mut kernel, 1);
    launch(&mut kernel, old);
    kernel.apply(old, Event::ResultObserved(RESULT)).unwrap();
    kernel.apply(old, Event::ExpireResult(RESULT)).unwrap();
    kernel.observe_generation_closed(scope(0)).unwrap();
    kernel.replace_generation(ScopeBinding::new(10, 1, 99)).unwrap();
    let before = kernel.clone();
    assert_eq!(kernel.accept(old.key, b"immutable command"), Ok(old));
    assert_eq!(kernel, before);
    rejected_unchanged(&mut kernel, old, Event::Authorize, Rejected::NotCurrentStage);
    rejected_unchanged(&mut kernel, old, Event::Go, Rejected::NotCurrentStage);
}

#[test]
fn highest_generation_survives_empty_scopes_and_owner_changes() {
    let mut kernel = Kernel::new(scope(5));
    kernel.observe_generation_closed(scope(5)).unwrap();
    let other = ScopeBinding::new(11, 0, 99);
    kernel.replace_generation(other).unwrap();
    kernel.observe_generation_closed(other).unwrap();
    for generation in [2, 5] {
        let before = kernel.clone();
        assert_eq!(
            kernel.replace_generation(ScopeBinding::new(10, generation, 100)),
            Err(Rejected::StaleGeneration)
        );
        assert_eq!(kernel, before);
    }
    kernel.replace_generation(ScopeBinding::new(10, 6, 100)).unwrap();
}

#[test]
fn old_generation_cleanup_never_changes_replacement_state() {
    let mut kernel = Kernel::new(scope(0));
    kernel.observe_generation_closed(scope(0)).unwrap();
    kernel.replace_generation(scope(1)).unwrap();
    let current = accepted(&mut kernel, 2);
    launch(&mut kernel, current);
    let before = kernel.clone();
    assert_eq!(kernel.observe_generation_closed(scope(0)), Err(Rejected::ScopeMismatch));
    assert_eq!(kernel, before);
}

#[test]
fn two_old_controls_cannot_change_later_pending_running_or_replacement_stage() {
    let controls = [
        Event::Cancel,
        Event::ResultObserved(RESULT),
        Event::Ack(RESULT),
        Event::StageClosed,
    ];
    for cut in 0..3 {
        let mut initial = Kernel::new(scope(0));
        let old = accepted(&mut initial, 1);
        launch(&mut initial, old);
        initial.apply(old, Event::StageClosed).unwrap();
        initial.apply(old, Event::ResultObserved(RESULT)).unwrap();
        if cut == 2 {
            initial.observe_generation_closed(scope(0)).unwrap();
            initial.replace_generation(scope(1)).unwrap();
        }
        let current = accepted(&mut initial, 2);
        launch(&mut initial, current);
        if cut != 0 {
            initial
                .apply(current, Event::SpawnObserved(SpawnFact::Spawned))
                .unwrap();
        }
        // Same result identity deliberately: isolation must come from the stage
        // binding, not an assumption that every stage's result ID differs.
        initial.apply(current, Event::ResultObserved(RESULT)).unwrap();
        for first in controls {
            for second in controls {
                let mut kernel = initial.clone();
                let before_scope = kernel.scope();
                let before_stage = kernel.stage(current.key);
                for event in [first, second] {
                    assert_eq!(kernel.apply(old, event), Ok(Effect::None), "cut={cut}, event={event:?}");
                    assert_eq!(kernel.scope(), before_scope);
                    assert_eq!(kernel.stage(current.key), before_stage);
                    assert_eq!(kernel.stage(old.key).unwrap().gate, LaunchGate::Consumed);
                }
            }
        }
    }
}

#[test]
fn first_historical_result_after_replacement_is_useful_but_never_changes_current_resources() {
    let mut kernel = Kernel::new(scope(0));
    let old = accepted(&mut kernel, 1);
    kernel.observe_generation_closed(scope(0)).unwrap();
    kernel.replace_generation(scope(1)).unwrap();
    let current = accepted(&mut kernel, 2);
    let before_scope = kernel.scope();
    let before_stage = kernel.stage(current.key);
    kernel.apply(old, Event::ResultObserved(RESULT)).unwrap();
    assert_eq!(kernel.stage(old.key).unwrap().result, Retention::Retained(RESULT));
    assert_eq!(kernel.scope(), before_scope);
    assert_eq!(kernel.stage(current.key), before_stage);
}

#[test]
fn duplicate_spawn_observation_after_closure_changes_no_state() {
    for fact in [SpawnFact::Spawned, SpawnFact::Failed] {
        let mut kernel = Kernel::new(scope(0));
        let stage = accepted(&mut kernel, 1);
        launch(&mut kernel, stage);
        kernel.apply(stage, Event::SpawnObserved(fact)).unwrap();
        kernel.apply(stage, Event::StageClosed).unwrap();
        let before = kernel.clone();
        kernel.apply(stage, Event::SpawnObserved(fact)).unwrap();
        assert_eq!(kernel, before);
    }
}

#[test]
fn failed_spawn_history_cannot_authorize_after_closure_and_new_stage() {
    let mut kernel = Kernel::new(scope(0));
    let old = accepted(&mut kernel, 1);
    launch(&mut kernel, old);
    kernel.apply(old, Event::SpawnObserved(SpawnFact::Failed)).unwrap();
    kernel.apply(old, Event::StageClosed).unwrap();
    let current = accepted(&mut kernel, 2);
    launch(&mut kernel, current);
    rejected_unchanged(&mut kernel, old, Event::Authorize, Rejected::NotCurrentStage);
    rejected_unchanged(&mut kernel, old, Event::Go, Rejected::NotCurrentStage);
}
