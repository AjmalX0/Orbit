//! Compiled `test` declarations, executed by `orbit test`.
//!
//! A [`TestCase`] is fully checked by the compiler: the workflow exists, the
//! input and signal payloads are constants of the right types, every mocked
//! task is used by the workflow (or one of its child workflows), and mock
//! results are expressions over the task's input typed by its contract.

use crate::expr::Expr;
use crate::workflow::SourceLocation;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestCase {
    pub name: String,
    pub workflow: String,
    pub version: String,
    /// The execution input, with defaults applied.
    pub input: Value,
    /// Mocked tasks by name. Each behaviour answers `times` calls of one unit
    /// (step, element or compensation) before the next one takes over; the
    /// last behaviour answers every remaining call.
    pub mocks: BTreeMap<String, Vec<MockBehaviour>>,
    /// Signals sent to the execution, in order.
    pub signals: Vec<TestSignal>,
    pub expectations: Vec<Expectation>,
    pub location: SourceLocation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MockBehaviour {
    pub outcome: MockOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub times: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MockOutcome {
    /// Complete with this value; `input` reads the task's input.
    Returns { output: Expr },
    /// Fail the attempt with a structured error.
    Fails {
        code: String,
        message: String,
        retryable: bool,
    },
    /// Never answer: the attempt's `timeout` (if any) decides what happens.
    Hangs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestSignal {
    pub name: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Expectation {
    /// The final workflow status (`COMPLETED`, `FAILED`, `CANCELLED`, `TIMED_OUT`).
    Status { status: String },
    /// The workflow output contains these values (see [`value_matches`]).
    Output { expected: Value },
    /// The workflow failed with this error code.
    Error { code: String },
    /// The unit of a step ended in this status.
    StepStatus { step: String, status: String },
    /// The step's output contains these values.
    StepOutput { step: String, expected: Value },
    /// The step took exactly this many attempts.
    StepAttempts { step: String, attempts: u32 },
    /// The step's compensation ran and succeeded.
    Compensated { step: String },
}

impl fmt::Display for Expectation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expectation::Status { status } => write!(f, "status {status}"),
            Expectation::Output { expected } => write!(f, "output {expected}"),
            Expectation::Error { code } => write!(f, "error {code:?}"),
            Expectation::StepStatus { step, status } => write!(f, "step {step} {status}"),
            Expectation::StepOutput { step, expected } => {
                write!(f, "step {step} output {expected}")
            }
            Expectation::StepAttempts { step, attempts } => {
                write!(f, "step {step} attempts {attempts}")
            }
            Expectation::Compensated { step } => write!(f, "compensated {step}"),
        }
    }
}

/// Workflow statuses an `expect status` may name.
pub const TEST_WORKFLOW_STATUSES: &[&str] = &["COMPLETED", "FAILED", "CANCELLED", "TIMED_OUT"];

/// Unit statuses an `expect step <name> <STATUS>` may name.
pub const TEST_STEP_STATUSES: &[&str] = &[
    "PENDING",
    "RUNNING",
    "SCHEDULED",
    "DISPATCHED",
    "ATTEMPT_FAILED",
    "UNCERTAIN",
    "RETRY_WAIT",
    "WAITING",
    "SUCCEEDED",
    "SKIPPED",
    "FAILED",
    "CANCELLED",
];

/// Partial match used by `expect output`: an expected object matches when
/// every field it lists matches the actual value's field (recursively);
/// fields it does not list are ignored. Anything else must be equal (numbers
/// compare by value, so `1` matches `1.0`).
pub fn value_matches(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::Object(want), Value::Object(have)) => want
            .iter()
            .all(|(k, v)| value_matches(v, have.get(k).unwrap_or(&Value::Null))),
        // Lists match element by element (same length), each element partially.
        (Value::Array(want), Value::Array(have)) => {
            want.len() == have.len() && want.iter().zip(have).all(|(w, h)| value_matches(w, h))
        }
        _ => crate::expr::values_equal(expected, actual),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn partial_matching() {
        let actual = json!({ "a": 1, "b": { "c": "x", "d": [1, 2] } });
        assert!(value_matches(&json!({}), &actual));
        assert!(value_matches(&json!({ "a": 1.0 }), &actual));
        assert!(value_matches(&json!({ "b": { "d": [1, 2] } }), &actual));
        assert!(!value_matches(&json!({ "b": { "d": [1] } }), &actual));
        let rows = json!([{ "q": "a", "n": 1 }, { "q": "b", "n": 2 }]);
        assert!(value_matches(&json!([{ "q": "a" }, { "q": "b" }]), &rows));
        assert!(!value_matches(&json!([{ "q": "a" }]), &rows));
        assert!(!value_matches(&json!([{ "q": "b" }, { "q": "a" }]), &rows));
        assert!(!value_matches(&json!({ "z": 1 }), &actual));
        assert!(value_matches(&json!({ "z": null }), &actual));
    }
}
