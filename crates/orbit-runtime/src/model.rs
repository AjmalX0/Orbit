//! Execution history events and the state derived from them.
//!
//! The event history is authoritative (RFC-0001 §9.8): [`ExecutionState`] is
//! always the left fold of [`apply`] over an execution's events, so a runtime
//! restart reconstructs exactly the state that was committed.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Structured task error, as reported by a worker or produced by the runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskError {
    pub code: String,
    pub message: String,
    #[serde(default = "yes")]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

fn yes() -> bool {
    true
}

impl TaskError {
    pub fn new(code: &str, message: impl Into<String>, retryable: bool) -> Self {
        TaskError {
            code: code.into(),
            message: message.into(),
            retryable,
            details: None,
        }
    }
}

/// Runtime-generated error codes.
pub mod codes {
    pub const TIMEOUT: &str = "TIMEOUT";
    pub const QUEUE_TIMEOUT: &str = "QUEUE_TIMEOUT";
    pub const EXPRESSION_ERROR: &str = "EXPRESSION_ERROR";
    pub const INPUT_CONTRACT_VIOLATION: &str = "INPUT_CONTRACT_VIOLATION";
    pub const OUTPUT_CONTRACT_VIOLATION: &str = "OUTPUT_CONTRACT_VIOLATION";
    pub const CANCELLED: &str = "CANCELLED";
    pub const WORKFLOW_TIMEOUT: &str = "WORKFLOW_TIMEOUT";
    pub const ADAPTER_DISABLED: &str = "ADAPTER_DISABLED";
    pub const HTTP_ERROR: &str = "HTTP_ERROR";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    WorkflowCreated {
        workflow: String,
        version: String,
        definition_hash: String,
        input: Value,
    },
    WorkflowStarted {},
    /// The unit is ready and queued for a worker. `input` is the fully evaluated payload.
    StepScheduled {
        step: String,
        attempt: u32,
        input: Value,
        idempotency_key: String,
        redelivery: bool,
    },
    StepSkipped {
        step: String,
        reason: String,
    },
    StepDispatched {
        step: String,
        attempt: u32,
        token: String,
        worker_id: String,
        worker_session: String,
        deadline_ms: Option<u64>,
    },
    StepSucceeded {
        step: String,
        attempt: u32,
        output: Value,
    },
    StepAttemptFailed {
        step: String,
        attempt: u32,
        error: TaskError,
    },
    /// The outcome of a dispatched attempt is unknown (worker lost, runtime restarted).
    /// The same attempt is redelivered with the same idempotency key.
    StepAttemptUncertain {
        step: String,
        attempt: u32,
        reason: String,
    },
    RetryScheduled {
        step: String,
        next_attempt: u32,
        delay_ms: u64,
        fire_at_ms: u64,
    },
    StepFailed {
        step: String,
        error: TaskError,
    },
    /// A queued unit was withdrawn because the workflow is being aborted.
    StepCancelled {
        step: String,
    },
    TimerStarted {
        step: String,
        fire_at_ms: u64,
    },
    TimerFired {
        step: String,
    },
    /// A result arrived for an attempt that no longer matters (duplicate or late).
    StaleResultIgnored {
        step: String,
        token: String,
        outcome: String,
    },
    CancelRequested {
        reason: String,
    },
    WorkflowTimeoutReached {},
    CompensationStarted {
        reason: String,
        steps: Vec<String>,
    },
    CompensationCompleted {},
    CompensationFailed {
        step: String,
        error: TaskError,
    },
    WorkflowCompleted {
        output: Value,
    },
    WorkflowFailed {
        error: TaskError,
        compensation: CompensationOutcome,
    },
    WorkflowCancelled {
        compensation: CompensationOutcome,
    },
    WorkflowTimedOut {
        compensation: CompensationOutcome,
    },
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::WorkflowCreated { .. } => "WorkflowCreated",
            Event::WorkflowStarted { .. } => "WorkflowStarted",
            Event::StepScheduled { .. } => "StepScheduled",
            Event::StepSkipped { .. } => "StepSkipped",
            Event::StepDispatched { .. } => "StepDispatched",
            Event::StepSucceeded { .. } => "StepSucceeded",
            Event::StepAttemptFailed { .. } => "StepAttemptFailed",
            Event::StepAttemptUncertain { .. } => "StepAttemptUncertain",
            Event::RetryScheduled { .. } => "RetryScheduled",
            Event::StepFailed { .. } => "StepFailed",
            Event::StepCancelled { .. } => "StepCancelled",
            Event::TimerStarted { .. } => "TimerStarted",
            Event::TimerFired { .. } => "TimerFired",
            Event::StaleResultIgnored { .. } => "StaleResultIgnored",
            Event::CancelRequested { .. } => "CancelRequested",
            Event::WorkflowTimeoutReached { .. } => "WorkflowTimeoutReached",
            Event::CompensationStarted { .. } => "CompensationStarted",
            Event::CompensationCompleted { .. } => "CompensationCompleted",
            Event::CompensationFailed { .. } => "CompensationFailed",
            Event::WorkflowCompleted { .. } => "WorkflowCompleted",
            Event::WorkflowFailed { .. } => "WorkflowFailed",
            Event::WorkflowCancelled { .. } => "WorkflowCancelled",
            Event::WorkflowTimedOut { .. } => "WorkflowTimedOut",
        }
    }

    pub fn step(&self) -> Option<&str> {
        match self {
            Event::StepScheduled { step, .. }
            | Event::StepSkipped { step, .. }
            | Event::StepDispatched { step, .. }
            | Event::StepSucceeded { step, .. }
            | Event::StepAttemptFailed { step, .. }
            | Event::StepAttemptUncertain { step, .. }
            | Event::RetryScheduled { step, .. }
            | Event::StepFailed { step, .. }
            | Event::StepCancelled { step }
            | Event::TimerStarted { step, .. }
            | Event::TimerFired { step }
            | Event::StaleResultIgnored { step, .. }
            | Event::CompensationFailed { step, .. } => Some(step),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompensationOutcome {
    /// Nothing needed compensating.
    None,
    Completed,
    Failed,
}

/// A recorded event with its position and commit time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEvent {
    pub seq: u64,
    pub timestamp_ms: u64,
    #[serde(flatten)]
    pub event: Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WorkflowStatus {
    Running,
    Compensating,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl WorkflowStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            WorkflowStatus::Completed
                | WorkflowStatus::Failed
                | WorkflowStatus::Cancelled
                | WorkflowStatus::TimedOut
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            WorkflowStatus::Running => "RUNNING",
            WorkflowStatus::Compensating => "COMPENSATING",
            WorkflowStatus::Completed => "COMPLETED",
            WorkflowStatus::Failed => "FAILED",
            WorkflowStatus::Cancelled => "CANCELLED",
            WorkflowStatus::TimedOut => "TIMED_OUT",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UnitStatus {
    Pending,
    /// Queued, waiting for a worker.
    Scheduled,
    Dispatched,
    /// Transient: an attempt failed and the decider will choose retry or failure.
    AttemptFailed,
    /// Transient: the outcome of the attempt is unknown and it will be redelivered.
    Uncertain,
    RetryWait,
    /// Durable timer running (`sleep`).
    Waiting,
    Succeeded,
    Skipped,
    Failed,
    Cancelled,
}

impl UnitStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            UnitStatus::Succeeded
                | UnitStatus::Skipped
                | UnitStatus::Failed
                | UnitStatus::Cancelled
        )
    }
}

/// Why a running workflow is being wound down.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AbortReason {
    Failed { error: TaskError },
    Cancelled { reason: String },
    TimedOut,
}

/// One schedulable unit: a workflow step, or the compensation of a step
/// (key `compensate:<step>`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UnitState {
    pub status: Option<UnitStatus>,
    pub attempt: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<TaskError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<TaskError>,
    pub redelivery: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduled_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatched_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_session: Option<String>,
    /// Token of the in-flight dispatch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Every token issued for this unit, with its attempt number. Results for
    /// any of them are accepted while the unit is unresolved (reconciliation).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tokens: Vec<(String, u32)>,
    /// Commit order of success, used to compensate in reverse.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub succeeded_seq: Option<u64>,
    pub dispatch_count: u32,
}

impl UnitState {
    pub fn status(&self) -> UnitStatus {
        self.status.unwrap_or(UnitStatus::Pending)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionState {
    pub id: String,
    pub workflow: String,
    pub version: String,
    pub definition_hash: String,
    pub input: Value,
    pub status: WorkflowStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    pub units: BTreeMap<String, UnitState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abort: Option<AbortReason>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compensation_order: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compensation: Option<CompensationOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<TaskError>,
    pub last_seq: u64,
}

pub fn compensation_key(step: &str) -> String {
    format!("compensate:{step}")
}

impl ExecutionState {
    pub fn new(id: &str) -> Self {
        ExecutionState {
            id: id.into(),
            workflow: String::new(),
            version: String::new(),
            definition_hash: String::new(),
            input: Value::Null,
            status: WorkflowStatus::Running,
            created_at_ms: 0,
            updated_at_ms: 0,
            finished_at_ms: None,
            units: BTreeMap::new(),
            abort: None,
            compensation_order: vec![],
            compensation: None,
            output: None,
            error: None,
            last_seq: 0,
        }
    }

    /// Rebuild from a full history.
    pub fn replay(id: &str, history: &[HistoryEvent]) -> Self {
        let mut s = ExecutionState::new(id);
        for h in history {
            apply(&mut s, h);
        }
        s
    }

    pub fn unit(&self, key: &str) -> Option<&UnitState> {
        self.units.get(key)
    }
}

/// Apply one event. Pure and total: unknown combinations are ignored so that
/// replaying an older history never panics.
pub fn apply(s: &mut ExecutionState, h: &HistoryEvent) {
    let ts = h.timestamp_ms;
    s.last_seq = h.seq;
    s.updated_at_ms = ts;
    match &h.event {
        Event::WorkflowCreated {
            workflow,
            version,
            definition_hash,
            input,
        } => {
            s.workflow = workflow.clone();
            s.version = version.clone();
            s.definition_hash = definition_hash.clone();
            s.input = input.clone();
            s.created_at_ms = ts;
            s.status = WorkflowStatus::Running;
        }
        Event::WorkflowStarted {} => {}
        Event::StepScheduled {
            step,
            attempt,
            input,
            idempotency_key,
            redelivery,
        } => {
            let u = s.units.entry(step.clone()).or_default();
            u.status = Some(UnitStatus::Scheduled);
            u.attempt = *attempt;
            u.input = Some(input.clone());
            u.idempotency_key = Some(idempotency_key.clone());
            u.redelivery = *redelivery;
            u.scheduled_at_ms = Some(ts);
            u.token = None;
            u.deadline_ms = None;
            u.wake_at_ms = None;
        }
        Event::StepSkipped { step, .. } => {
            let u = s.units.entry(step.clone()).or_default();
            u.status = Some(UnitStatus::Skipped);
            u.output = Some(Value::Null);
            u.finished_at_ms = Some(ts);
        }
        Event::StepDispatched {
            step,
            attempt,
            token,
            worker_id,
            worker_session,
            deadline_ms,
        } => {
            let u = s.units.entry(step.clone()).or_default();
            u.status = Some(UnitStatus::Dispatched);
            u.attempt = *attempt;
            u.token = Some(token.clone());
            u.tokens.push((token.clone(), *attempt));
            u.worker_id = Some(worker_id.clone());
            u.worker_session = Some(worker_session.clone());
            u.dispatched_at_ms = Some(ts);
            u.deadline_ms = *deadline_ms;
            u.dispatch_count += 1;
        }
        Event::StepSucceeded {
            step,
            attempt,
            output,
        } => {
            let u = s.units.entry(step.clone()).or_default();
            u.status = Some(UnitStatus::Succeeded);
            u.attempt = *attempt;
            u.output = Some(output.clone());
            u.finished_at_ms = Some(ts);
            u.succeeded_seq = Some(h.seq);
            u.token = None;
            u.wake_at_ms = None;
        }
        Event::StepAttemptFailed { step, error, .. } => {
            let u = s.units.entry(step.clone()).or_default();
            u.status = Some(UnitStatus::AttemptFailed);
            u.last_error = Some(error.clone());
            u.token = None;
        }
        Event::StepAttemptUncertain { step, .. } => {
            let u = s.units.entry(step.clone()).or_default();
            u.status = Some(UnitStatus::Uncertain);
            u.token = None;
        }
        Event::RetryScheduled {
            step,
            next_attempt,
            fire_at_ms,
            ..
        } => {
            let u = s.units.entry(step.clone()).or_default();
            u.status = Some(UnitStatus::RetryWait);
            u.attempt = *next_attempt;
            u.wake_at_ms = Some(*fire_at_ms);
        }
        Event::StepFailed { step, error } => {
            let u = s.units.entry(step.clone()).or_default();
            u.status = Some(UnitStatus::Failed);
            u.error = Some(error.clone());
            u.finished_at_ms = Some(ts);
            u.token = None;
            if s.abort.is_none() && !step.starts_with("compensate:") {
                s.abort = Some(AbortReason::Failed {
                    error: TaskError {
                        message: format!("step `{step}` failed: {}", error.message),
                        ..error.clone()
                    },
                });
            }
        }
        Event::StepCancelled { step } => {
            let u = s.units.entry(step.clone()).or_default();
            u.status = Some(UnitStatus::Cancelled);
            u.finished_at_ms = Some(ts);
            u.token = None;
        }
        Event::TimerStarted { step, fire_at_ms } => {
            let u = s.units.entry(step.clone()).or_default();
            u.status = Some(UnitStatus::Waiting);
            u.wake_at_ms = Some(*fire_at_ms);
            u.scheduled_at_ms = Some(ts);
            u.attempt = 1;
        }
        Event::TimerFired { step } => {
            let u = s.units.entry(step.clone()).or_default();
            u.status = Some(UnitStatus::Succeeded);
            u.output = Some(Value::Object(Default::default()));
            u.finished_at_ms = Some(ts);
            u.succeeded_seq = Some(h.seq);
            u.wake_at_ms = None;
        }
        Event::StaleResultIgnored { .. } => {}
        Event::CancelRequested { reason } => {
            if s.abort.is_none() {
                s.abort = Some(AbortReason::Cancelled {
                    reason: reason.clone(),
                });
            }
        }
        Event::WorkflowTimeoutReached {} => {
            if s.abort.is_none() {
                s.abort = Some(AbortReason::TimedOut);
            }
        }
        Event::CompensationStarted { steps, .. } => {
            s.status = WorkflowStatus::Compensating;
            s.compensation_order = steps.clone();
            for st in steps {
                s.units.entry(compensation_key(st)).or_default();
            }
        }
        Event::CompensationCompleted {} => s.compensation = Some(CompensationOutcome::Completed),
        Event::CompensationFailed { .. } => s.compensation = Some(CompensationOutcome::Failed),
        Event::WorkflowCompleted { output } => {
            s.status = WorkflowStatus::Completed;
            s.output = Some(output.clone());
            s.finished_at_ms = Some(ts);
        }
        Event::WorkflowFailed {
            error,
            compensation,
        } => {
            s.status = WorkflowStatus::Failed;
            s.error = Some(error.clone());
            s.compensation = Some(*compensation);
            s.finished_at_ms = Some(ts);
        }
        Event::WorkflowCancelled { compensation } => {
            s.status = WorkflowStatus::Cancelled;
            s.compensation = Some(*compensation);
            s.finished_at_ms = Some(ts);
        }
        Event::WorkflowTimedOut { compensation } => {
            s.status = WorkflowStatus::TimedOut;
            s.compensation = Some(*compensation);
            s.finished_at_ms = Some(ts);
        }
    }
}
