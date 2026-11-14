//! The compiled workflow document (`orbit-ir/1`).
//!
//! The IR is the contract between compiler and runtime. It is fully resolved:
//! named schemas are inlined, step dependencies are explicit (including those
//! implied by data references), and every expression is typed and checked.

use crate::expr::Expr;
use crate::types::Type;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

pub const IR_VERSION: &str = "orbit-ir/1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowIr {
    pub ir_version: String,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `sha256:<hex>` over the semantic content (excludes `source_map`).
    #[serde(default)]
    pub definition_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    pub input: Type,
    pub output: Type,
    /// Steps in a deterministic topological order.
    pub steps: Vec<StepIr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Expr>,
    /// Every worker contract referenced by this workflow.
    pub contracts: BTreeMap<String, TaskContract>,
    /// Source locations for diagnostics and tooling (not hashed).
    #[serde(default)]
    pub source_map: BTreeMap<String, SourceLocation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceLocation {
    pub file: String,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskContract {
    pub name: String,
    pub version: String,
    pub input: Type,
    pub output: Type,
    /// True for tasks executed inside the runtime (e.g. `orbit.http.request`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub builtin: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepIr {
    pub id: String,
    pub action: Action,
    /// Direct dependencies, sorted. Explicit `after` plus data references.
    pub dependencies: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<Expr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensation: Option<Invocation>,
    /// The type other expressions see when they reference this step.
    pub output_type: Type,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Task(Invocation),
    /// Durable timer handled by the runtime.
    Sleep { duration_ms: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invocation {
    pub task: String,
    pub contract_version: String,
    /// Optional routing constraint: only workers announcing this runtime receive the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub input: Expr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<Expr>,
    /// Start-to-close limit for one attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Maximum time the task may wait in the queue before a worker picks it up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_timeout_ms: Option<u64>,
    pub retry: RetryPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backoff {
    Fixed,
    Exponential,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum total attempts, including the first.
    pub max_attempts: u32,
    pub backoff: Backoff,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    /// Error codes that fail the step immediately.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub non_retryable: Vec<String>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy { max_attempts: 1, backoff: Backoff::Fixed, initial_delay_ms: 1000, max_delay_ms: 60_000, non_retryable: vec![] }
    }
}

impl RetryPolicy {
    /// Delay before attempt `next_attempt` (2 = first retry).
    pub fn delay_before(&self, next_attempt: u32) -> u64 {
        match self.backoff {
            Backoff::Fixed => self.initial_delay_ms.min(self.max_delay_ms.max(self.initial_delay_ms)),
            Backoff::Exponential => {
                let exp = next_attempt.saturating_sub(2).min(40);
                self.initial_delay_ms.saturating_mul(1u64 << exp).min(self.max_delay_ms)
            }
        }
    }
}

impl StepIr {
    pub fn invocation(&self) -> Option<&Invocation> {
        match &self.action {
            Action::Task(i) => Some(i),
            Action::Sleep { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrError(pub String);

impl fmt::Display for IrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for IrError {}

impl WorkflowIr {
    pub fn step(&self, id: &str) -> Option<&StepIr> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// Canonical hash of the semantic content of the workflow.
    pub fn compute_hash(&self) -> String {
        let mut copy = self.clone();
        copy.definition_hash = String::new();
        copy.source_map.clear();
        let bytes = serde_json::to_vec(&copy).expect("IR serialises");
        let digest = Sha256::digest(&bytes);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        format!("sha256:{hex}")
    }

    pub fn seal(mut self) -> Self {
        self.definition_hash = self.compute_hash();
        self
    }

    pub fn from_json(text: &str) -> Result<WorkflowIr, IrError> {
        // Check the version before attempting a full parse so that future IR
        // versions produce a clear error instead of a confusing schema error.
        let raw: serde_json::Value = serde_json::from_str(text).map_err(|e| IrError(format!("invalid IR JSON: {e}")))?;
        match raw.get("ir_version").and_then(|v| v.as_str()) {
            Some(IR_VERSION) => {}
            Some(other) => return Err(IrError(format!("unsupported IR version `{other}`; this runtime supports `{IR_VERSION}`"))),
            None => return Err(IrError("missing `ir_version`".into())),
        }
        let ir: WorkflowIr = serde_json::from_value(raw).map_err(|e| IrError(format!("malformed IR: {e}")))?;
        ir.validate()?;
        Ok(ir)
    }

    /// Enforce the IR invariants from RFC-0001 §7.3. The runtime calls this on
    /// every definition it accepts, so a hand-edited IR cannot bypass them.
    pub fn validate(&self) -> Result<(), IrError> {
        let e = |m: String| Err(IrError(m));
        if self.ir_version != IR_VERSION {
            return e(format!("unsupported IR version `{}`", self.ir_version));
        }
        if self.name.is_empty() || self.version.is_empty() {
            return e("workflow name and version are required".into());
        }
        if !matches!(self.input, Type::Record { .. }) {
            return e("workflow input must be a record type".into());
        }
        let mut ids = HashSet::new();
        for s in &self.steps {
            if !ids.insert(s.id.as_str()) {
                return e(format!("duplicate step id `{}`", s.id));
            }
        }
        let mut seen = HashSet::new();
        for s in &self.steps {
            for d in &s.dependencies {
                if !ids.contains(d.as_str()) {
                    return e(format!("step `{}` depends on unknown step `{d}`", s.id));
                }
                // Steps are in topological order, so dependencies must already be seen.
                if !seen.contains(d.as_str()) {
                    return e(format!("dependency graph is cyclic or not topologically ordered at `{}` -> `{d}`", s.id));
                }
            }
            let mut refs = vec![];
            if let Some(c) = &s.condition {
                c.step_refs(&mut refs);
            }
            if let Some(inv) = s.invocation() {
                inv.input.step_refs(&mut refs);
                if let Some(k) = &inv.idempotency_key {
                    k.step_refs(&mut refs);
                }
            }
            let ancestors = self.ancestors(&s.id);
            for r in &refs {
                if !ancestors.contains(r.as_str()) {
                    return e(format!("step `{}` reads `{r}` which is not one of its dependencies", s.id));
                }
            }
            for inv in s.invocation().into_iter().chain(s.compensation.as_ref()) {
                match self.contracts.get(&inv.task) {
                    None => return e(format!("step `{}` uses task `{}` with no contract", s.id, inv.task)),
                    Some(c) if c.version != inv.contract_version => {
                        return e(format!("step `{}` expects {}@{} but contract is {}", s.id, inv.task, inv.contract_version, c.version))
                    }
                    _ => {}
                }
                if inv.retry.max_attempts == 0 {
                    return e(format!("step `{}`: retry attempts must be at least 1", s.id));
                }
            }
            seen.insert(s.id.as_str());
        }
        if !self.definition_hash.is_empty() && self.definition_hash != self.compute_hash() {
            return e("definition_hash does not match the IR content (was the file modified?)".into());
        }
        Ok(())
    }

    /// Transitive dependencies of a step.
    pub fn ancestors(&self, id: &str) -> HashSet<&str> {
        let by_id: HashMap<&str, &StepIr> = self.steps.iter().map(|s| (s.id.as_str(), s)).collect();
        let mut out = HashSet::new();
        let mut stack: Vec<&str> = by_id.get(id).map(|s| s.dependencies.iter().map(String::as_str).collect()).unwrap_or_default();
        while let Some(n) = stack.pop() {
            if out.insert(n) {
                if let Some(s) = by_id.get(n) {
                    stack.extend(s.dependencies.iter().map(String::as_str));
                }
            }
        }
        out
    }
}
