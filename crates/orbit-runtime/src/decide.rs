//! The deterministic workflow decider.
//!
//! `decide(state, definition, now)` inspects an execution and returns the
//! events that should happen next (schedule ready steps, fan out `for` steps,
//! fire timers, apply retry policy, start compensation, finish the
//! workflow...). It performs no I/O. The engine applies the events, persists
//! them, and calls `decide` again until it returns nothing.
//!
//! Unit keys: `step` for a step, `step[i]` for element `i` of a `for` step,
//! and `compensate:<unit>` for the compensation of a unit.

use crate::model::*;
use orbit_ir::{eval, Action, Invocation, Scope, StepIr, WorkflowIr};
use serde_json::Value;

/// Expression scope over an execution, optionally with a loop variable bound
/// and with one step's output overridden (an element's own output inside its
/// compensation).
pub struct StateScope<'a> {
    pub state: &'a ExecutionState,
    pub local: Option<(&'a str, &'a Value)>,
    pub own_output: Option<(&'a str, &'a Value)>,
}

impl<'a> StateScope<'a> {
    pub fn new(state: &'a ExecutionState) -> Self {
        StateScope {
            state,
            local: None,
            own_output: None,
        }
    }
}

impl Scope for StateScope<'_> {
    fn input(&self) -> &Value {
        &self.state.input
    }

    fn step_output(&self, step: &str) -> Option<&Value> {
        if let Some((s, v)) = self.own_output {
            if s == step {
                return Some(v);
            }
        }
        let u = self.state.units.get(step)?;
        match u.status() {
            UnitStatus::Succeeded | UnitStatus::Skipped => u.output.as_ref(),
            _ => None,
        }
    }

    fn execution_field(&self, field: &str) -> Option<Value> {
        Some(Value::String(match field {
            "id" => self.state.id.clone(),
            "workflow" => self.state.workflow.clone(),
            "version" => self.state.version.clone(),
            _ => return None,
        }))
    }

    fn local(&self, name: &str) -> Option<&Value> {
        self.local.filter(|(n, _)| *n == name).map(|(_, v)| v)
    }
}

/// The invocation behind a unit key.
pub fn invocation<'a>(def: &'a WorkflowIr, key: &str) -> Option<&'a Invocation> {
    let k = parse_key(key);
    let step = def.step(k.step)?;
    if k.compensation {
        step.compensation.as_ref()
    } else {
        step.invocation()
    }
}

fn unit(state: &ExecutionState, key: &str) -> UnitState {
    state.units.get(key).cloned().unwrap_or_default()
}

/// The `for` element bound to a unit, if any: (variable, value).
fn element<'a>(
    state: &'a ExecutionState,
    def: &'a WorkflowIr,
    key: &str,
) -> Option<(&'a str, &'a Value)> {
    let k = parse_key(key);
    let index = k.index?;
    let fe = def.step(k.step)?.for_each.as_ref()?;
    let items = state.units.get(k.step)?.input.as_ref()?.as_array()?;
    Some((fe.variable.as_str(), items.get(index)?))
}

/// Evaluate the payload and idempotency key for a unit's first attempt.
fn schedule(state: &ExecutionState, def: &WorkflowIr, key: &str, inv: &Invocation) -> Event {
    let k = parse_key(key);
    let mut scope = StateScope::new(state);
    scope.local = element(state, def, key);
    // Inside the compensation of an element, `<step>` is that element's output.
    let element_unit = if k.compensation && k.index.is_some() {
        key.strip_prefix("compensate:")
    } else {
        None
    };
    let own = element_unit
        .and_then(|e| state.units.get(e))
        .and_then(|u| u.output.as_ref());
    if let Some(v) = own {
        scope.own_output = Some((k.step, v));
    }
    let fail = |code: &str, msg: String| Event::StepFailed {
        step: key.into(),
        error: TaskError::new(code, msg, false),
    };
    let mut input = match eval(&inv.input, &scope) {
        Ok(v) => v,
        Err(e) => return fail(codes::EXPRESSION_ERROR, format!("evaluating input: {e}")),
    };
    if let Some(contract) = def.contracts.get(&inv.task) {
        contract.input.apply_defaults(&mut input);
        if let Err(e) = contract.input.validate(&input, false) {
            return fail(
                codes::INPUT_CONTRACT_VIOLATION,
                format!("input for `{}` is invalid at {e}", inv.task),
            );
        }
    }
    let idempotency_key = match &inv.idempotency_key {
        Some(expr) => match eval(expr, &scope) {
            Ok(Value::String(s)) => s,
            Ok(Value::Null) => format!("{}/{key}", state.id),
            Ok(other) => other.to_string(),
            Err(e) => {
                return fail(
                    codes::EXPRESSION_ERROR,
                    format!("evaluating idempotency_key: {e}"),
                )
            }
        },
        None => format!("{}/{key}", state.id),
    };
    Event::StepScheduled {
        step: key.into(),
        attempt: 1,
        input,
        idempotency_key,
        redelivery: false,
    }
}

/// Retry-or-fail decision after a failed attempt.
fn after_failure(
    state: &ExecutionState,
    key: &str,
    u: &UnitState,
    inv: Option<&Invocation>,
    now: u64,
) -> Event {
    let error = u
        .last_error
        .clone()
        .unwrap_or_else(|| TaskError::new("UNKNOWN", "attempt failed", true));
    let is_compensation = key.starts_with("compensate:");
    let aborting = state.abort.is_some() && !is_compensation;
    if let Some(inv) = inv {
        let policy = &inv.retry;
        let retryable = error.retryable && !policy.non_retryable.contains(&error.code);
        if retryable && !aborting && u.attempt < policy.max_attempts {
            let next = u.attempt + 1;
            let delay = policy.delay_before(next);
            return Event::RetryScheduled {
                step: key.into(),
                next_attempt: next,
                delay_ms: delay,
                fire_at_ms: now + delay,
            };
        }
        if retryable && u.attempt >= policy.max_attempts && policy.max_attempts > 1 {
            let mut e = error.clone();
            e.message = format!("{} (gave up after {} attempts)", e.message, u.attempt);
            return Event::StepFailed {
                step: key.into(),
                error: e,
            };
        }
    }
    Event::StepFailed {
        step: key.into(),
        error,
    }
}

fn finish(state: &ExecutionState, outcome: CompensationOutcome) -> Event {
    match &state.abort {
        Some(AbortReason::Cancelled { .. }) => Event::WorkflowCancelled {
            compensation: outcome,
        },
        Some(AbortReason::TimedOut) => Event::WorkflowTimedOut {
            compensation: outcome,
        },
        Some(AbortReason::Failed { error }) => Event::WorkflowFailed {
            error: error.clone(),
            compensation: outcome,
        },
        None => Event::WorkflowFailed {
            error: TaskError::new("UNKNOWN", "workflow aborted", false),
            compensation: outcome,
        },
    }
}

/// Compute the next events. Returns an empty vector at a fixpoint.
pub fn decide(state: &ExecutionState, def: &WorkflowIr, now: u64) -> Vec<Event> {
    if state.status.is_terminal() {
        return vec![];
    }
    let mut out = vec![];

    // 1. Workflow deadline.
    if state.status == WorkflowStatus::Running && state.abort.is_none() {
        if let Some(t) = def.timeout_ms {
            if now >= state.created_at_ms + t {
                return vec![Event::WorkflowTimeoutReached {}];
            }
        }
    }

    // 2. Per-unit transitions: deadlines, timers, retries, redelivery.
    // Signals delivered in this pass (so two waiting units never take the same one).
    let mut taken: Vec<u64> = vec![];
    for (key, u) in &state.units {
        let inv = invocation(def, key);
        match u.status() {
            UnitStatus::Dispatched => {
                if u.deadline_ms.is_some_and(|d| now >= d) {
                    out.push(Event::StepAttemptFailed {
                        step: key.clone(),
                        attempt: u.attempt,
                        error: TaskError::new(
                            codes::TIMEOUT,
                            format!("attempt {} exceeded its timeout", u.attempt),
                            true,
                        ),
                    });
                }
            }
            UnitStatus::Scheduled => {
                let qt = inv.and_then(|i| i.queue_timeout_ms);
                if let (Some(qt), Some(at)) = (qt, u.scheduled_at_ms) {
                    if !u.redelivery && now >= at + qt {
                        out.push(Event::StepAttemptFailed {
                            step: key.clone(),
                            attempt: u.attempt,
                            error: TaskError::new(
                                codes::QUEUE_TIMEOUT,
                                "no worker picked up the task before queue_timeout",
                                false,
                            ),
                        });
                    }
                }
            }
            UnitStatus::Waiting => {
                if let Some(signal) = awaited_signal(def, key) {
                    // `await`: take the oldest undelivered signal of this name.
                    let next = state.signals.iter().find(|r| {
                        r.name == signal && r.delivered_to.is_none() && !taken.contains(&r.seq)
                    });
                    if let Some(r) = next {
                        taken.push(r.seq);
                        out.push(Event::SignalDelivered {
                            step: key.clone(),
                            signal_seq: r.seq,
                        });
                    } else if u.wake_at_ms.is_some_and(|w| now >= w) {
                        out.push(Event::StepFailed {
                            step: key.clone(),
                            error: TaskError::new(
                                codes::SIGNAL_TIMEOUT,
                                format!("no `{signal}` signal arrived before the step's timeout"),
                                false,
                            ),
                        });
                    }
                } else if u.wake_at_ms.is_some_and(|w| now >= w) {
                    out.push(Event::TimerFired { step: key.clone() });
                }
            }
            UnitStatus::RetryWait => {
                if u.wake_at_ms.is_some_and(|w| now >= w) {
                    if state.abort.is_some() && !key.starts_with("compensate:") {
                        out.push(Event::StepFailed {
                            step: key.clone(),
                            error: u.last_error.clone().unwrap_or_else(|| {
                                TaskError::new(codes::CANCELLED, "aborted", false)
                            }),
                        });
                    } else {
                        out.push(Event::StepScheduled {
                            step: key.clone(),
                            attempt: u.attempt,
                            input: u.input.clone().unwrap_or(Value::Null),
                            idempotency_key: u.idempotency_key.clone().unwrap_or_default(),
                            redelivery: false,
                        });
                    }
                }
            }
            UnitStatus::AttemptFailed => out.push(after_failure(state, key, u, inv, now)),
            UnitStatus::Uncertain => out.push(Event::StepScheduled {
                step: key.clone(),
                attempt: u.attempt,
                input: u.input.clone().unwrap_or(Value::Null),
                idempotency_key: u.idempotency_key.clone().unwrap_or_default(),
                redelivery: true,
            }),
            _ => {}
        }
    }
    if !out.is_empty() {
        return out;
    }

    match state.status {
        WorkflowStatus::Running => decide_running(state, def, now),
        WorkflowStatus::Compensating => decide_compensating(state, def),
        _ => vec![],
    }
}

/// The signal an `await` unit waits for.
fn awaited_signal<'a>(def: &'a WorkflowIr, key: &str) -> Option<&'a str> {
    let k = parse_key(key);
    if k.compensation {
        return None;
    }
    match &def.step(k.step)?.action {
        Action::Await { signal, .. } => Some(signal),
        _ => None,
    }
}

/// Evaluate a `compute` step (or one element of it) in the runtime.
fn compute(state: &ExecutionState, def: &WorkflowIr, key: &str, expr: &orbit_ir::Expr) -> Event {
    let mut scope = StateScope::new(state);
    scope.local = element(state, def, key);
    match eval(expr, &scope) {
        Ok(output) => Event::StepSucceeded {
            step: key.into(),
            attempt: 1,
            output,
        },
        Err(e) => Event::StepFailed {
            step: key.into(),
            error: TaskError::new(
                codes::EXPRESSION_ERROR,
                format!("evaluating `compute`: {e}"),
                false,
            ),
        },
    }
}

fn deps_done(state: &ExecutionState, step: &StepIr) -> bool {
    step.dependencies.iter().all(|d| {
        matches!(
            unit(state, d).status(),
            UnitStatus::Succeeded | UnitStatus::Skipped
        )
    })
}

/// Element unit keys of a fanned-out step, in order.
fn element_keys(state: &ExecutionState, step: &str) -> Vec<String> {
    let n = state
        .units
        .get(step)
        .and_then(|u| u.input.as_ref())
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    (0..n).map(|i| element_key(step, i)).collect()
}

fn in_flight(status: UnitStatus) -> bool {
    matches!(
        status,
        UnitStatus::Scheduled
            | UnitStatus::Dispatched
            | UnitStatus::AttemptFailed
            | UnitStatus::Uncertain
            | UnitStatus::RetryWait
    )
}

/// Progress a running `for` step: schedule elements within `max_parallel`,
/// and resolve the step once every element is done (or one failed).
fn advance_fan_out(state: &ExecutionState, def: &WorkflowIr, step: &StepIr, out: &mut Vec<Event>) {
    let keys = element_keys(state, &step.id);
    let units: Vec<UnitState> = keys.iter().map(|k| unit(state, k)).collect();
    if let Some(failed) = units.iter().find(|u| u.status() == UnitStatus::Failed) {
        out.push(Event::StepFailed {
            step: step.id.clone(),
            error: failed
                .error
                .clone()
                .unwrap_or_else(|| TaskError::new("UNKNOWN", "element failed", false)),
        });
        return;
    }
    if units.iter().all(|u| u.status() == UnitStatus::Succeeded) {
        let output = Value::Array(
            units
                .iter()
                .map(|u| u.output.clone().unwrap_or(Value::Null))
                .collect(),
        );
        out.push(Event::StepSucceeded {
            step: step.id.clone(),
            attempt: 1,
            output,
        });
        return;
    }
    if let Action::Compute { expr } = &step.action {
        for (key, u) in keys.iter().zip(&units) {
            if u.status() == UnitStatus::Pending {
                out.push(compute(state, def, key, expr));
            }
        }
        return;
    }
    let Some(inv) = step.invocation() else { return };
    let limit = step
        .for_each
        .as_ref()
        .and_then(|f| f.max_parallel)
        .map(|n| n as usize)
        .unwrap_or(usize::MAX);
    let mut running = units.iter().filter(|u| in_flight(u.status())).count();
    for (key, u) in keys.iter().zip(&units) {
        if running >= limit {
            break;
        }
        if u.status() == UnitStatus::Pending {
            out.push(schedule(state, def, key, inv));
            running += 1;
        }
    }
}

fn decide_running(state: &ExecutionState, def: &WorkflowIr, now: u64) -> Vec<Event> {
    let mut out = vec![];
    if state.abort.is_none() {
        for step in &def.steps {
            let u = unit(state, &step.id);
            if u.status() == UnitStatus::Running {
                advance_fan_out(state, def, step, &mut out);
                continue;
            }
            if u.status() != UnitStatus::Pending || !deps_done(state, step) {
                continue;
            }
            if let Some(cond) = &step.condition {
                match eval(cond, &StateScope::new(state)) {
                    Ok(Value::Bool(true)) => {}
                    Ok(Value::Bool(false)) | Ok(Value::Null) => {
                        out.push(Event::StepSkipped {
                            step: step.id.clone(),
                            reason: "`when` condition was false".into(),
                        });
                        continue;
                    }
                    Ok(other) => {
                        out.push(Event::StepFailed {
                            step: step.id.clone(),
                            error: TaskError::new(
                                codes::EXPRESSION_ERROR,
                                format!("`when` evaluated to {other}, expected Bool"),
                                false,
                            ),
                        });
                        continue;
                    }
                    Err(e) => {
                        out.push(Event::StepFailed {
                            step: step.id.clone(),
                            error: TaskError::new(
                                codes::EXPRESSION_ERROR,
                                format!("evaluating `when`: {e}"),
                                false,
                            ),
                        });
                        continue;
                    }
                }
            }
            if let Some(fe) = &step.for_each {
                match eval(&fe.items, &StateScope::new(state)) {
                    Ok(Value::Array(items)) => out.push(Event::StepFanOut {
                        step: step.id.clone(),
                        items,
                    }),
                    Ok(Value::Null) => out.push(Event::StepFanOut {
                        step: step.id.clone(),
                        items: vec![],
                    }),
                    Ok(other) => out.push(Event::StepFailed {
                        step: step.id.clone(),
                        error: TaskError::new(
                            codes::EXPRESSION_ERROR,
                            format!("`for` expected a list, got {other}"),
                            false,
                        ),
                    }),
                    Err(e) => out.push(Event::StepFailed {
                        step: step.id.clone(),
                        error: TaskError::new(
                            codes::EXPRESSION_ERROR,
                            format!("evaluating `for`: {e}"),
                            false,
                        ),
                    }),
                }
                continue;
            }
            match &step.action {
                Action::Sleep { duration_ms } => out.push(Event::TimerStarted {
                    step: step.id.clone(),
                    fire_at_ms: now + duration_ms,
                }),
                Action::Task(inv) | Action::Workflow(inv) => {
                    out.push(schedule(state, def, &step.id, inv))
                }
                Action::Await { signal, timeout_ms } => out.push(Event::SignalAwaitStarted {
                    step: step.id.clone(),
                    signal: signal.clone(),
                    deadline_ms: timeout_ms.map(|t| now + t),
                }),
                Action::Compute { expr } => out.push(compute(state, def, &step.id, expr)),
            }
        }
        if !out.is_empty() {
            return out;
        }
        let all_done = def
            .steps
            .iter()
            .all(|s| unit(state, &s.id).status().is_terminal());
        if all_done {
            let output = match &def.result {
                Some(expr) => match eval(expr, &StateScope::new(state)) {
                    Ok(v) => v,
                    Err(e) => {
                        return vec![Event::WorkflowFailed {
                            error: TaskError::new(
                                codes::EXPRESSION_ERROR,
                                format!("evaluating `return`: {e}"),
                                false,
                            ),
                            compensation: CompensationOutcome::None,
                        }]
                    }
                },
                None => Value::Object(Default::default()),
            };
            if let Err(e) = def.output.validate(&output, false) {
                return vec![Event::WorkflowFailed {
                    error: TaskError::new(
                        codes::OUTPUT_CONTRACT_VIOLATION,
                        format!("workflow output is invalid at {e}"),
                        false,
                    ),
                    compensation: CompensationOutcome::None,
                }];
            }
            return vec![Event::WorkflowCompleted { output }];
        }
        return out;
    }

    // Aborting: withdraw work that has not started, wait for in-flight work,
    // then compensate completed units in reverse completion order.
    let mut waiting = false;
    let withdraw =
        |key: &str, u: &UnitState, out: &mut Vec<Event>, waiting: &mut bool| match u.status() {
            UnitStatus::Pending | UnitStatus::Waiting | UnitStatus::RetryWait => {
                out.push(Event::StepCancelled { step: key.into() })
            }
            UnitStatus::Scheduled if !u.redelivery => {
                out.push(Event::StepCancelled { step: key.into() })
            }
            UnitStatus::Scheduled
            | UnitStatus::Dispatched
            | UnitStatus::AttemptFailed
            | UnitStatus::Uncertain => *waiting = true,
            _ => {}
        };
    for step in &def.steps {
        let u = unit(state, &step.id);
        if u.status() == UnitStatus::Running {
            let mut element_waiting = false;
            let mut failed = None;
            for key in element_keys(state, &step.id) {
                let eu = unit(state, &key);
                if eu.status() == UnitStatus::Failed && failed.is_none() {
                    failed = eu.error.clone();
                }
                withdraw(&key, &eu, &mut out, &mut element_waiting);
            }
            if element_waiting {
                waiting = true;
            } else {
                // Every element is resolved: close the step itself.
                out.push(match failed {
                    Some(error) => Event::StepFailed {
                        step: step.id.clone(),
                        error,
                    },
                    None => Event::StepCancelled {
                        step: step.id.clone(),
                    },
                });
            }
        } else {
            withdraw(&step.id, &u, &mut out, &mut waiting);
        }
    }
    if !out.is_empty() || waiting {
        return out;
    }
    // Units that succeeded and have a compensation: whole steps, or each
    // succeeded element of a `for` step.
    let mut done: Vec<(String, u64)> = vec![];
    for s in def.steps.iter().filter(|s| s.compensation.is_some()) {
        let keys = if s.for_each.is_some() {
            element_keys(state, &s.id)
        } else {
            vec![s.id.clone()]
        };
        for key in keys {
            if let Some(u) = state.units.get(&key) {
                if u.status() == UnitStatus::Succeeded {
                    done.push((key, u.succeeded_seq.unwrap_or(0)));
                }
            }
        }
    }
    done.sort_by_key(|d| std::cmp::Reverse(d.1));
    if done.is_empty() {
        return vec![finish(state, CompensationOutcome::None)];
    }
    let reason = match &state.abort {
        Some(AbortReason::Failed { error }) => error.message.clone(),
        Some(AbortReason::Cancelled { reason }) => format!("cancelled: {reason}"),
        Some(AbortReason::TimedOut) => "workflow timed out".into(),
        None => String::new(),
    };
    vec![Event::CompensationStarted {
        reason,
        steps: done.into_iter().map(|(s, _)| s).collect(),
    }]
}

fn decide_compensating(state: &ExecutionState, def: &WorkflowIr) -> Vec<Event> {
    // Compensations run one at a time, newest completed unit first (Saga).
    for step in &state.compensation_order {
        let key = compensation_key(step);
        let u = unit(state, &key);
        match u.status() {
            UnitStatus::Succeeded => continue,
            UnitStatus::Failed | UnitStatus::Cancelled => {
                let error = u
                    .error
                    .unwrap_or_else(|| TaskError::new("UNKNOWN", "compensation failed", false));
                return vec![
                    Event::CompensationFailed {
                        step: step.clone(),
                        error,
                    },
                    finish(state, CompensationOutcome::Failed),
                ];
            }
            UnitStatus::Pending => {
                return match invocation(def, &key) {
                    Some(inv) => vec![schedule(state, def, &key, inv)],
                    None => vec![Event::StepFailed {
                        step: key,
                        error: TaskError::new("UNKNOWN", "missing compensation", false),
                    }],
                };
            }
            _ => return vec![],
        }
    }
    vec![
        Event::CompensationCompleted {},
        finish(state, CompensationOutcome::Completed),
    ]
}
