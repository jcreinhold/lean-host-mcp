//! Client ordering only: two independent commits, no process-custody authority.
use super::store::{Binding, CallerJournal, CallerResult, NativeStore, Problem, Reject, SavedRequest};

/// Complete the trusted caller's save and acknowledgment in that order.
pub(crate) fn accept_then_ack(
    caller: &mut CallerJournal,
    native: &mut NativeStore,
    request: &SavedRequest,
    expected: Binding,
) -> Result<CallerResult, Problem> {
    if expected.key != request.key {
        return Err(Problem::Rejected(Reject::CallerBindingChanged));
    }
    if caller.get(&request.caller)?.as_ref() != Some(request) {
        return Err(Problem::Rejected(Reject::CallerBindingChanged));
    }
    let source = native
        .get(request.key)?
        .ok_or(Problem::Rejected(Reject::ResultUnavailable))?;
    if source.key != request.key || !source.matches_intent(&request.intent) || source.scope != Some(expected.scope) {
        return Err(Problem::Rejected(Reject::WrongScope));
    }
    // save_result first recovers an existing matching caller copy, including
    // when native bytes have expired. No caller transaction survives this call.
    let accepted = caller.save_result(request, &source)?;
    native.ack(expected, &request.capability, accepted.result_id())?;
    Ok(accepted)
}
