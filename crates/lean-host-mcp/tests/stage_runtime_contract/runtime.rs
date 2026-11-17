use crate::fixtures::{Dispatch, Fixture, GoFault, NativeStage, Observation};
use crate::kernel::{
    Effect, Event, Kernel, Rejected, Resources, ResultId, ScopeBinding, ScopeSnapshot, StageBinding, StageKey,
    StageSnapshot,
};

#[derive(Clone)]
pub(crate) struct LaunchPacket {
    binding: StageBinding,
}
#[derive(Clone)]
pub(crate) struct StopPacket {
    binding: StageBinding,
}
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum PrepareProblem {
    Kernel(Rejected),
    OneShotGenerationAlreadyUsed,
}
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ReplaceProblem {
    Kernel(Rejected),
    NativeClosurePending,
    NativeObservationUnavailable,
}
#[derive(Clone, Copy)]
pub(crate) enum PollFault {
    None,
    LoseClosureObservation,
}
#[derive(Default)]
pub(crate) struct PollReport {
    pub(crate) observations: Vec<Observation>,
    pub(crate) problems: Vec<String>,
}

// No mutable-kernel escape hatch: closure observations must come from NativeStage.
// This test adapter has one one-shot physical context, not a warm-stage scheduler.
pub(crate) struct Runtime {
    kernel: Kernel,
    native: Option<NativeStage>,
}
impl Runtime {
    pub(crate) fn new(scope: ScopeBinding) -> Self {
        Self {
            kernel: Kernel::new(scope),
            native: None,
        }
    }
    pub(crate) fn scope(&self) -> ScopeSnapshot {
        self.kernel.scope()
    }
    pub(crate) fn stage(&self, key: StageKey) -> Option<StageSnapshot> {
        self.kernel.stage(key)
    }
    pub(crate) fn native(&self) -> &NativeStage {
        self.native.as_ref().expect("prepared native context")
    }
    pub(crate) fn prepare(
        &mut self,
        fixture: &Fixture,
        key: StageKey,
        intent: &[u8],
    ) -> Result<StageBinding, PrepareProblem> {
        // Check immutable history BEFORE any native factory call. A retry never
        // creates another root, even after result expiry/generation replacement.
        if self.kernel.stage(key).is_some() {
            return self.kernel.accept(key, intent).map_err(PrepareProblem::Kernel);
        }
        if self.native.is_some() {
            return Err(PrepareProblem::OneShotGenerationAlreadyUsed);
        }
        let binding = self.kernel.accept(key, intent).map_err(PrepareProblem::Kernel)?;
        self.native = Some(NativeStage::prepare(fixture, key, binding, self.kernel.scope().binding));
        Ok(binding)
    }
    pub(crate) fn request_start(&mut self, binding: StageBinding) -> Result<Option<LaunchPacket>, Rejected> {
        self.kernel.apply(binding, Event::Authorize)?;
        match self.kernel.apply(binding, Event::Go)? {
            Effect::None => Ok(None),
            Effect::Launch(target) if target == binding => Ok(Some(LaunchPacket { binding })),
            Effect::Launch(_) | Effect::RequestStop(_) => Err(Rejected::BindingMismatch),
        }
    }
    pub(crate) fn deliver_launch(&mut self, packet: &LaunchPacket, fault: GoFault) -> Dispatch {
        let Some(native) = self.native.as_mut() else {
            return Dispatch::Stale;
        };
        if native.binding() != packet.binding {
            return Dispatch::Stale;
        }
        native.send_go(fault)
    }
    pub(crate) fn request_cancel(&mut self, binding: StageBinding) -> Result<Option<StopPacket>, Rejected> {
        match self.kernel.apply(binding, Event::Cancel)? {
            Effect::None => Ok(None),
            Effect::RequestStop(target) if target == binding => Ok(Some(StopPacket { binding })),
            Effect::RequestStop(_) | Effect::Launch(_) => Err(Rejected::BindingMismatch),
        }
    }
    pub(crate) fn deliver_stop(&mut self, packet: &StopPacket, lose_delivery: bool) -> Dispatch {
        let Some(native) = self.native.as_mut() else {
            return Dispatch::Stale;
        };
        if native.binding() != packet.binding {
            return Dispatch::Stale;
        }
        if lose_delivery {
            return Dispatch::Unconfirmed;
        }
        if native.request_stop().is_err() {
            Dispatch::Unconfirmed
        } else {
            Dispatch::Sent
        }
    }
    pub(crate) fn poll(&mut self, fault: PollFault) -> PollReport {
        let mut report = PollReport::default();
        let Some(native) = self.native.as_mut() else {
            return report;
        };
        // One source of stop intent. Lost StopPacket delivery does not erase
        // Stopping. Reconciliation retries cleanup, never the target launch.
        if self
            .kernel
            .stage(native.key())
            .is_some_and(|stage| stage.binding == native.binding() && stage.resources == Resources::Stopping)
            && let Err(error) = native.request_stop()
        {
            report.problems.push(format!("stop request unconfirmed: {error}"));
        }
        match native.observations() {
            Ok(events) => report.observations.extend(events),
            Err(error) => report.problems.push(format!("result observation unavailable: {error}")),
        }
        // A bad/lost result observation cannot prevent independent resource
        // observation. Neither error invents a closure or semantic result.
        match native.closed_observations() {
            Ok(events) if matches!(fault, PollFault::LoseClosureObservation) && !events.is_empty() => report
                .problems
                .push("injected loss of genuine closure observation".into()),
            Ok(events) => report.observations.extend(events),
            Err(error) => report.problems.push(format!("native closure uncertain: {error}")),
        }
        report
    }
    pub(crate) fn deliver_observation(&mut self, event: &Observation) -> Result<(), Rejected> {
        event.deliver(&mut self.kernel)
    }
    pub(crate) fn ack(&mut self, binding: StageBinding, result: ResultId) -> Result<(), Rejected> {
        self.kernel.apply(binding, Event::Ack(result)).map(|_| ())
    }
    pub(crate) fn expire(&mut self, binding: StageBinding, result: ResultId) -> Result<(), Rejected> {
        self.kernel.apply(binding, Event::ExpireResult(result)).map(|_| ())
    }
    pub(crate) fn replace(&mut self, next: ScopeBinding) -> Result<(), ReplaceProblem> {
        if let Some(native) = self.native.as_mut() {
            let closed = native
                .closed_observations()
                .map_err(|_| ReplaceProblem::NativeObservationUnavailable)?;
            if closed.is_empty() {
                return Err(ReplaceProblem::NativeClosurePending);
            }
        }
        self.kernel.replace_generation(next).map_err(ReplaceProblem::Kernel)?;
        drop(self.native.take());
        Ok(())
    }
}
