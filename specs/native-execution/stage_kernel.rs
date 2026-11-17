//! Executable reference kernel for stage identity and late-message isolation.
//!
//! Not wired into the server. Observation events are trusted inputs whose
//! authenticity, complete scope closure and persistence must be established by
//! a future runtime. One emitted Launch is not proof of one OS spawn.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct AttemptId {
    store: u128,
    acceptance_epoch: u64,
    nonce: u128,
}
impl AttemptId {
    pub(crate) const fn new(store: u128, acceptance_epoch: u64, nonce: u128) -> Self {
        Self {
            store,
            acceptance_epoch,
            nonce,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct StageKey {
    attempt: AttemptId,
    stage: u64,
}
impl StageKey {
    pub(crate) const fn new(attempt: AttemptId, stage: u64) -> Self {
        Self { attempt, stage }
    }
}

/// Owner is a logical incarnation, never a PID or a signalling capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScopeBinding {
    scope: u128,
    generation: u64,
    owner: u128,
}
impl ScopeBinding {
    pub(crate) const fn new(scope: u128, generation: u64, owner: u128) -> Self {
        Self {
            scope,
            generation,
            owner,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StageBinding {
    key: StageKey,
    scope: ScopeBinding,
}

/// Opaque retained-fact identity; this is not a semantic success verdict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResultId(pub(crate) u128);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LaunchGate {
    AwaitingAuthorization,
    Authorized,
    Consumed,
    Sealed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpawnFact {
    Spawned,
    Failed,
}
/// Stage association only, not ownership of any kernel lock or physical resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Resources {
    Open,
    Stopping,
    Closed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Retention {
    Pending,
    Retained(ResultId),
    Acknowledged(ResultId),
    Expired(ResultId),
}
impl Retention {
    const fn id(self) -> Option<ResultId> {
        match self {
            Self::Pending => None,
            Self::Retained(id) | Self::Acknowledged(id) | Self::Expired(id) => Some(id),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StageSnapshot {
    pub(crate) binding: StageBinding,
    pub(crate) gate: LaunchGate,
    pub(crate) spawn: Option<SpawnFact>,
    pub(crate) resources: Resources,
    pub(crate) result: Retention,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScopeSnapshot {
    pub(crate) binding: ScopeBinding,
    pub(crate) active: Option<StageKey>,
    pub(crate) closed: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct StageRecord {
    intent: Box<[u8]>,
    snapshot: StageSnapshot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Event {
    Authorize,
    Go,
    SpawnObserved(SpawnFact),
    Cancel,
    /// Runtime must have sealed every pending launch AND closed all stage work.
    StageClosed,
    ResultObserved(ResultId),
    Ack(ResultId),
    ExpireResult(ResultId),
}
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Effect {
    None,
    Launch(StageBinding),
    RequestStop(StageBinding),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Rejected {
    IntentChanged,
    Busy,
    GenerationClosed,
    UnknownStage,
    BindingMismatch,
    NotCurrentStage,
    NotAuthorized,
    NoConsumedGate,
    ConflictingSpawn,
    ResultUnknown,
    ConflictingResult,
    ScopeMismatch,
    ScopeStillOpen,
    StaleGeneration,
}

/// One in-memory scope and immutable stage history. No durable restore/GC API.
#[derive(Debug, Eq, PartialEq)]
#[cfg_attr(test, derive(Clone))]
pub(crate) struct Kernel {
    current: ScopeSnapshot,
    highest_generation: BTreeMap<u128, u64>,
    stages: BTreeMap<StageKey, StageRecord>,
}
impl Kernel {
    pub(crate) fn new(scope: ScopeBinding) -> Self {
        Self {
            current: ScopeSnapshot {
                binding: scope,
                active: None,
                closed: false,
            },
            highest_generation: BTreeMap::from([(scope.scope, scope.generation)]),
            stages: BTreeMap::new(),
        }
    }
    pub(crate) const fn scope(&self) -> ScopeSnapshot {
        self.current
    }
    pub(crate) fn stage(&self, key: StageKey) -> Option<StageSnapshot> {
        self.stages.get(&key).map(|record| record.snapshot)
    }

    /// Equal retries return the original binding without reactivating work.
    pub(crate) fn accept(&mut self, key: StageKey, intent: &[u8]) -> Result<StageBinding, Rejected> {
        if let Some(record) = self.stages.get(&key) {
            return if record.intent.as_ref() == intent {
                Ok(record.snapshot.binding)
            } else {
                Err(Rejected::IntentChanged)
            };
        }
        if self.current.closed {
            return Err(Rejected::GenerationClosed);
        }
        if self.current.active.is_some() {
            return Err(Rejected::Busy);
        }
        let binding = StageBinding {
            key,
            scope: self.current.binding,
        };
        self.stages.insert(
            key,
            StageRecord {
                intent: intent.into(),
                snapshot: StageSnapshot {
                    binding,
                    gate: LaunchGate::AwaitingAuthorization,
                    spawn: None,
                    resources: Resources::Open,
                    result: Retention::Pending,
                },
            },
        );
        self.current.active = Some(key);
        Ok(binding)
    }

    /// Historical events must match their immutable original binding. Only GO
    /// and authorization additionally require the current active generation.
    pub(crate) fn apply(&mut self, binding: StageBinding, event: Event) -> Result<Effect, Rejected> {
        let record = self.stages.get_mut(&binding.key).ok_or(Rejected::UnknownStage)?;
        if record.snapshot.binding != binding {
            return Err(Rejected::BindingMismatch);
        }
        let stage = &mut record.snapshot;
        match event {
            Event::Authorize | Event::Go => {
                if self.current.binding != binding.scope
                    || self.current.closed
                    || self.current.active != Some(binding.key)
                {
                    return Err(Rejected::NotCurrentStage);
                }
                match (event, stage.gate) {
                    (Event::Authorize, LaunchGate::AwaitingAuthorization) => {
                        stage.gate = LaunchGate::Authorized;
                    }
                    (Event::Go, LaunchGate::AwaitingAuthorization) => {
                        return Err(Rejected::NotAuthorized);
                    }
                    (Event::Go, LaunchGate::Authorized) => {
                        stage.gate = LaunchGate::Consumed; // before returning a launch effect
                        return Ok(Effect::Launch(binding));
                    }
                    _ => {} // duplicate or permanently sealed: never reopen
                }
            }
            Event::SpawnObserved(fact) => {
                if stage.gate != LaunchGate::Consumed {
                    return Err(Rejected::NoConsumedGate);
                }
                match stage.spawn {
                    Some(previous) if previous != fact => return Err(Rejected::ConflictingSpawn),
                    Some(_) => {}
                    None => stage.spawn = Some(fact),
                }
            }
            Event::Cancel => {
                if stage.resources != Resources::Closed && stage.resources != Resources::Stopping {
                    if stage.gate != LaunchGate::Consumed {
                        stage.gate = LaunchGate::Sealed;
                    }
                    stage.resources = Resources::Stopping;
                    return Ok(Effect::RequestStop(binding));
                }
            }
            Event::StageClosed => {
                close_stage(stage);
                if self.current.binding == binding.scope && self.current.active == Some(binding.key) {
                    self.current.active = None;
                }
            }
            Event::ResultObserved(id) => match stage.result.id() {
                None => stage.result = Retention::Retained(id),
                Some(previous) if previous != id => return Err(Rejected::ConflictingResult),
                Some(_) => {} // ACK/expiry cannot be undone by a duplicate report
            },
            Event::Ack(id) | Event::ExpireResult(id) => {
                match stage.result.id() {
                    None => return Err(Rejected::ResultUnknown),
                    Some(previous) if previous != id => return Err(Rejected::ConflictingResult),
                    Some(_) => {}
                }
                match (event, stage.result) {
                    (Event::Ack(_), Retention::Retained(_)) => {
                        stage.result = Retention::Acknowledged(id);
                    }
                    (Event::ExpireResult(_), _) => stage.result = Retention::Expired(id),
                    _ => {}
                }
            }
        }
        Ok(Effect::None)
    }

    /// Strong external observation: no work OR pending launch in this generation
    /// can run later. A quiet process census alone does not establish this input.
    pub(crate) fn observe_generation_closed(&mut self, scope: ScopeBinding) -> Result<(), Rejected> {
        if self.current.binding != scope {
            return Err(Rejected::ScopeMismatch);
        }
        for record in self.stages.values_mut() {
            if record.snapshot.binding.scope == scope {
                close_stage(&mut record.snapshot);
            }
        }
        self.current.active = None;
        self.current.closed = true;
        Ok(())
    }

    /// Numbers increase per stable scope ID, even after other scopes intervene.
    /// The future runtime must durably allocate/recover this identity history.
    pub(crate) fn replace_generation(&mut self, next: ScopeBinding) -> Result<(), Rejected> {
        if !self.current.closed {
            return Err(Rejected::ScopeStillOpen);
        }
        if self
            .highest_generation
            .get(&next.scope)
            .is_some_and(|highest| next.generation <= *highest)
        {
            return Err(Rejected::StaleGeneration);
        }
        self.highest_generation.insert(next.scope, next.generation);
        self.current = ScopeSnapshot {
            binding: next,
            active: None,
            closed: false,
        };
        Ok(())
    }
}
fn close_stage(stage: &mut StageSnapshot) {
    stage.resources = Resources::Closed;
    if stage.gate != LaunchGate::Consumed {
        stage.gate = LaunchGate::Sealed;
    }
}

#[cfg(test)]
#[path = "stage_kernel_tests.rs"]
mod tests;
