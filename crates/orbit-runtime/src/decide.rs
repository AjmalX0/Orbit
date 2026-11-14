//! The deterministic workflow decider.
//!
//! `decide(state, definition, now)` inspects an execution and returns the
//! events that should happen next (schedule ready steps, fire timers, apply
//! retry policy, start compensation, finish the workflow...). It performs no
//! I/O. The engine applies the events, persists them, and calls `decide`
//! again until it returns nothing.

use crate::model::*;
use orbit_ir::{eval, Action, Invocation, Scope, StepIr, WorkflowIr};
use serde_json::Value;

pub struct StateScope<'a> {
    pub state: &'a ExecutionState,
}

impl Scope for StateScope<'_> {
    fn input(&self) -> &Value {
        &self.state.input
    }

    fn step_output(&self, step: &str) -> Option<&Value> {
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
}

/// The invocation behind a unit key (`step` or `compensate:step`).
pub fn invocation<'a>(def: &'a WorkflowIr, key: &str) -> Option<&'a Invocation> {
    match key.strip_prefix("compensate:") {
        Some(step) => def.step(step)?.compensation.as_ref(),
        None => def.step(key)?.invocation(),
    }
}

fn unit(state: &ExecutionState, key: &str) -> UnitState {
    state.units.get(key).cloned().unwrap_or_default()
}

/// Evaluate the payload and idempotency key for a unit's first attempt.
fn schedule(state: &ExecutionState, def: &WorkflowIr, key: &str, inv: &Invocation) -> Event {
    let scope = StateScope { state };
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
        Some(k) => match eval(k, &scope) {
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
                if u.wake_at_ms.is_some_and(|w| now >= w) {
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

fn deps_done(state: &ExecutionState, step: &StepIr) -> bool {
    step.dependencies.iter().all(|d| {
        matches!(
            unit(state, d).status(),
            UnitStatus::Succeeded | UnitStatus::Skipped
        )
    })
}

fn decide_running(state: &ExecutionState, def: &WorkflowIr, now: u64) -> Vec<Event> {
    let mut out = vec![];
    if state.abort.is_none() {
        for step in &def.steps {
            let u = unit(state, &step.id);
            if u.status() != UnitStatus::Pending || !deps_done(state, step) {
                continue;
            }
            if let Some(cond) = &step.condition {
                match eval(cond, &StateScope { state }) {
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
            match &step.action {
                Action::Sleep { duration_ms } => out.push(Event::TimerStarted {
                    step: step.id.clone(),
                    fire_at_ms: now + duration_ms,
                }),
                Action::Task(inv) => out.push(schedule(state, def, &step.id, inv)),
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
                Some(expr) => match eval(expr, &StateScope { state }) {
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
    // then compensate completed steps in reverse completion order.
    let mut in_flight = false;
    for step in &def.steps {
        let u = unit(state, &step.id);
        match u.status() {
            UnitStatus::Pending | UnitStatus::Waiting | UnitStatus::RetryWait => {
                out.push(Event::StepCancelled {
                    step: step.id.clone(),
                })
            }
            UnitStatus::Scheduled if !u.redelivery => out.push(Event::StepCancelled {
                step: step.id.clone(),
            }),
            UnitStatus::Scheduled
            | UnitStatus::Dispatched
            | UnitStatus::AttemptFailed
            | UnitStatus::Uncertain => in_flight = true,
            _ => {}
        }
    }
    if !out.is_empty() || in_flight {
        return out;
    }
    let mut done: Vec<(&str, u64)> = def
        .steps
        .iter()
        .filter(|s| s.compensation.is_some())
        .filter_map(|s| {
            let u = state.units.get(&s.id)?;
            (u.status() == UnitStatus::Succeeded)
                .then(|| (s.id.as_str(), u.succeeded_seq.unwrap_or(0)))
        })
        .collect();
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
        steps: done.into_iter().map(|(s, _)| s.to_string()).collect(),
    }]
}

fn decide_compensating(state: &ExecutionState, def: &WorkflowIr) -> Vec<Event> {
    // Compensations run one at a time, newest completed step first (Saga).
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
