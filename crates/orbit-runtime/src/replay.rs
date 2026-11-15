//! Deterministic replay: re-run a recorded history through the current
//! decider and check that every decision the runtime recorded is reproduced.
//!
//! History events have two origins:
//!
//! * **External** events record something the decider cannot know: an
//!   execution was created, a task was handed to a worker, a worker reported
//!   a result, a signal or cancellation arrived, a worker was lost. Replay
//!   applies them as recorded.
//! * **Decided** events are produced by [`decide`] from the state and the
//!   clock. Replay recomputes `decide(state, definition, timestamp)` at each
//!   of them and requires the recorded event to be among the results (order
//!   within one decision batch does not matter).
//!
//! Some event types can be either: `StepSucceeded` is decided for `compute`
//! steps and finished `for` steps but reported by a worker for task steps,
//! and `StepAttemptFailed` is decided for deadlines but reported by workers
//! for task errors. Such an event is consistent if the decider produces it
//! or if it is a legitimate worker result for an unresolved task unit.
//!
//! Times computed from the clock (`fire_at_ms`, `deadline_ms`) may differ
//! by the few milliseconds between the runtime reading its clock and
//! committing the batch, so they are compared with that much tolerance.

use crate::decide::{decide, invocation};
use crate::model::*;
use orbit_ir::WorkflowIr;
use serde::Serialize;
use serde_json::Value;

/// Where a recorded event came from, as determined by replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    External,
    Decided,
    Divergent,
}

/// The origin of one recorded event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EventOrigin {
    pub seq: u64,
    pub origin: Origin,
}

/// A recorded decision the current decider does not reproduce.
#[derive(Debug, Clone, Serialize)]
pub struct Divergence {
    pub seq: u64,
    pub recorded: Event,
    /// What `decide` returns at that point instead.
    pub decider: Vec<Event>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplayReport {
    pub execution_id: String,
    pub events: usize,
    pub external: usize,
    pub decided: usize,
    pub divergences: Vec<Divergence>,
    /// The origin of every event, by position in the history.
    pub origins: Vec<EventOrigin>,
    /// Set when the history was recorded against a different definition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition_mismatch: Option<(String, String)>,
    /// What the decider would do next after the last event, if anything
    /// (for an unfinished execution: the work it is waiting on).
    pub pending: Vec<Event>,
    /// The state after the last event.
    pub state: ExecutionState,
}

impl ReplayReport {
    pub fn consistent(&self) -> bool {
        self.divergences.is_empty()
    }
}

/// Events that only ever come from outside the decider.
fn always_external(e: &Event) -> bool {
    matches!(
        e,
        Event::WorkflowCreated { .. }
            | Event::WorkflowStarted { .. }
            | Event::StepDispatched { .. }
            | Event::StepAttemptUncertain { .. }
            | Event::SignalReceived { .. }
            | Event::CancelRequested { .. }
            | Event::StaleResultIgnored { .. }
            // Server resource limits are configuration, not decisions: the
            // engine records them in place of what the decider proposed.
            | Event::LimitExceeded { .. }
    ) || matches!(e, Event::StepFailed { error, .. } if error.code == crate::model::codes::LIMIT_EXCEEDED)
}

/// True if `e` is a result the engine accepts from a worker (or a child
/// workflow, or a built-in task) for the unit's current state.
fn worker_result(state: &ExecutionState, def: &WorkflowIr, e: &Event) -> bool {
    let step = match e {
        Event::StepSucceeded { step, .. } | Event::StepAttemptFailed { step, .. } => step,
        _ => return false,
    };
    if invocation(def, step).is_none() {
        return false;
    }
    let status = state
        .units
        .get(step)
        .map(UnitState::status)
        .unwrap_or(UnitStatus::Pending);
    matches!(
        status,
        UnitStatus::Dispatched
            | UnitStatus::Scheduled
            | UnitStatus::RetryWait
            | UnitStatus::Uncertain
            | UnitStatus::AttemptFailed
    )
}

/// Clock-derived fields, compared with tolerance.
const TIME_FIELDS: &[&str] = &["fire_at_ms", "deadline_ms"];

/// Does the decided event `d` match the recorded event `r`? Clock-derived
/// times may be up to `slack` ms later in `d` (decided at the commit time)
/// than in `r` (decided when the runtime read its clock).
fn same(d: &Event, r: &Event, slack: u64) -> bool {
    let (Ok(Value::Object(mut dv)), Ok(Value::Object(mut rv))) =
        (serde_json::to_value(d), serde_json::to_value(r))
    else {
        return false;
    };
    for f in TIME_FIELDS {
        match (dv.remove(*f), rv.remove(*f)) {
            (Some(a), Some(b)) => match (a.as_u64(), b.as_u64()) {
                (Some(a), Some(b)) => {
                    if a < b || a - b > slack {
                        return false;
                    }
                }
                _ if a != b => return false,
                _ => {}
            },
            (None, None) => {}
            _ => return false,
        }
    }
    dv == rv
}

fn take(pending: &mut Vec<Event>, recorded: &Event, slack: u64) -> bool {
    match pending.iter().position(|d| same(d, recorded, slack)) {
        Some(i) => {
            pending.remove(i);
            true
        }
        None => false,
    }
}

/// Replay `history` (of execution `id`) against `def`. With `stop_at`, the
/// replay stops right after the event with that sequence number, so the
/// report's `state` is the execution as it was at that moment.
pub fn replay(
    id: &str,
    history: &[HistoryEvent],
    def: &WorkflowIr,
    stop_at: Option<u64>,
) -> ReplayReport {
    let mut state = ExecutionState::new(id);
    let mut report = ReplayReport {
        execution_id: id.to_string(),
        events: 0,
        external: 0,
        decided: 0,
        divergences: vec![],
        origins: vec![],
        definition_mismatch: None,
        pending: vec![],
        state: state.clone(),
    };
    // Events the decider produced in its last call that are not recorded yet.
    let mut pending: Vec<Event> = vec![];
    let mut prev_ts = history.first().map(|h| h.timestamp_ms).unwrap_or(0);
    for h in history {
        report.events += 1;
        if let Event::WorkflowCreated {
            definition_hash, ..
        } = &h.event
        {
            if *definition_hash != def.definition_hash {
                report.definition_mismatch =
                    Some((definition_hash.clone(), def.definition_hash.clone()));
            }
        }
        let slack = h.timestamp_ms.saturating_sub(prev_ts);
        let origin = if always_external(&h.event) {
            pending.clear();
            Origin::External
        } else if take(&mut pending, &h.event, slack) || {
            // A new decision batch. The runtime read its clock somewhere
            // between the previous commit and this one; try both ends.
            pending = decide(&state, def, h.timestamp_ms);
            take(&mut pending, &h.event, slack) || {
                let earlier = decide(&state, def, prev_ts);
                if earlier.iter().any(|d| same(d, &h.event, slack)) {
                    pending = earlier;
                    take(&mut pending, &h.event, slack)
                } else {
                    false
                }
            }
        } {
            Origin::Decided
        } else if worker_result(&state, def, &h.event) {
            pending.clear();
            Origin::External
        } else {
            report.divergences.push(Divergence {
                seq: h.seq,
                recorded: h.event.clone(),
                decider: decide(&state, def, h.timestamp_ms),
            });
            pending.clear();
            Origin::Divergent
        };
        match origin {
            Origin::External => report.external += 1,
            Origin::Decided => report.decided += 1,
            Origin::Divergent => {}
        }
        report.origins.push(EventOrigin { seq: h.seq, origin });
        apply(&mut state, h);
        prev_ts = h.timestamp_ms;
        if stop_at == Some(h.seq) {
            break;
        }
    }
    report.pending = decide(&state, def, prev_ts);
    report.state = state;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{simulate, TaskCall, TaskHandler, TaskOutcome};
    use serde_json::json;

    const SRC: &str = r#"
task "charge" { version "1.0.0" input { amount: Float } output { id: String } }
task "refund" { version "1.0.0" input { id: String } output { ok: Bool } }
task "ship" { version "1.0.0" input { n: Int } output { ok: Bool } }

workflow Order {
    version "1.0.0"
    input { amount: Float, items: List<Int> }
    signal go { ok: Bool }
    step assess { compute { big: input.amount > 100.0 } }
    step approval { await go  when assess.big  timeout 1h }
    step pause { sleep 5m  after approval }
    step charge {
        worker "charge"
        after pause
        with { amount: input.amount }
        retry { attempts: 3, initial_delay: 2s }
        compensate { worker "refund" with { id: charge.id } }
    }
    step ship {
        worker "ship"
        for n in input.items
        with { n: n }
        after charge
    }
    return { id: charge.id }
}
"#;

    /// `charge` fails once, `ship` fails for item 3 (so `charge` is refunded).
    struct Workers;
    impl TaskHandler for Workers {
        fn handle(&mut self, call: &TaskCall) -> TaskOutcome {
            let a = call.assignment;
            match a.task_name.as_str() {
                "charge" if call.call == 1 => {
                    TaskOutcome::Fail(TaskError::new("BUSY", "try again", true))
                }
                "charge" => TaskOutcome::Complete(json!({ "id": "tx-1" })),
                "ship" if a.input["n"] == 3 => {
                    TaskOutcome::Fail(TaskError::new("NO_STOCK", "out of stock", false))
                }
                "ship" => TaskOutcome::Complete(json!({ "ok": true })),
                _ => TaskOutcome::Complete(json!({ "ok": true })),
            }
        }
    }

    fn recorded() -> (WorkflowIr, Vec<HistoryEvent>) {
        let c = orbit_compiler::compile_str("order.orbit", SRC);
        assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
        let sim = simulate(
            &c.workflows,
            "Order",
            None,
            json!({ "amount": 250.0, "items": [1, 2, 3] }),
            &[("go".into(), json!({ "ok": true }))],
            &mut Workers,
        )
        .unwrap();
        assert_eq!(
            sim.root.state.status,
            WorkflowStatus::Failed,
            "{:?}",
            sim.problems
        );
        (c.workflows[0].clone(), sim.root.history)
    }

    #[test]
    fn a_recorded_history_replays_cleanly() {
        let (def, history) = recorded();
        let r = replay("sim", &history, &def, None);
        assert!(r.consistent(), "{:#?}", r.divergences);
        assert_eq!(r.events, history.len());
        assert_eq!(r.external + r.decided, history.len());
        assert!(
            r.decided > 10 && r.external > 5,
            "{} {}",
            r.decided,
            r.external
        );
        assert!(r.definition_mismatch.is_none());
        assert!(r.pending.is_empty());
        assert_eq!(r.state.status, WorkflowStatus::Failed);
        // Worker results for task steps are external; compute results are decided.
        let origin_of = |pred: &dyn Fn(&Event) -> bool| {
            let h = history.iter().find(|h| pred(&h.event)).unwrap();
            r.origins.iter().find(|o| o.seq == h.seq).unwrap().origin
        };
        assert_eq!(
            origin_of(&|e| matches!(e, Event::StepSucceeded { step, .. } if step == "assess")),
            Origin::Decided
        );
        assert_eq!(
            origin_of(&|e| matches!(e, Event::StepSucceeded { step, .. } if step == "charge")),
            Origin::External
        );
    }

    #[test]
    fn stopping_at_a_sequence_number_shows_the_state_then() {
        let (def, history) = recorded();
        let delivered = history
            .iter()
            .find(|h| matches!(h.event, Event::SignalDelivered { .. }))
            .unwrap()
            .seq;
        let r = replay("sim", &history, &def, Some(delivered));
        assert_eq!(r.events as u64, delivered);
        assert_eq!(r.state.status, WorkflowStatus::Running);
        assert_eq!(r.state.units["approval"].status(), UnitStatus::Succeeded);
        assert_eq!(
            r.state.units["approval"].output,
            Some(json!({ "ok": true }))
        );
        assert!(!r.state.units.contains_key("charge"));
        // The decider would start the timer next.
        assert!(
            matches!(r.pending[..], [Event::TimerStarted { .. }]),
            "{:?}",
            r.pending
        );
    }

    #[test]
    fn a_tampered_history_is_reported() {
        let (def, mut history) = recorded();
        // Pretend the runtime skipped the approval although the order is big.
        let i = history
            .iter()
            .position(|h| matches!(h.event, Event::SignalAwaitStarted { .. }))
            .unwrap();
        history[i].event = Event::StepSkipped {
            step: "approval".into(),
            reason: "`when` condition was false".into(),
        };
        let seq = history[i].seq;
        let r = replay("sim", &history, &def, None);
        assert!(!r.consistent());
        let d = &r.divergences[0];
        assert_eq!(d.seq, seq);
        assert!(
            matches!(d.decider[..], [Event::SignalAwaitStarted { .. }]),
            "{:?}",
            d.decider
        );

        // A changed retry delay is a divergence too.
        let (def, mut history) = recorded();
        for h in &mut history {
            if let Event::RetryScheduled { delay_ms, .. } = &mut h.event {
                *delay_ms = 9_000;
            }
        }
        let r = replay("sim", &history, &def, None);
        assert!(r
            .divergences
            .iter()
            .any(|d| matches!(d.recorded, Event::RetryScheduled { .. })));
    }

    #[test]
    fn a_worker_result_is_external_but_a_forged_compute_result_is_not() {
        let (def, mut history) = recorded();
        for h in &mut history {
            if let Event::StepSucceeded { step, output, .. } = &mut h.event {
                if step == "assess" {
                    *output = json!({ "big": false });
                }
            }
        }
        let r = replay("sim", &history, &def, None);
        // The first divergence is the forged result (later decisions then
        // follow from it, so they diverge too).
        assert!(!r.consistent());
        assert!(
            matches!(&r.divergences[0].recorded, Event::StepSucceeded { step, .. } if step == "assess")
        );
    }
}
