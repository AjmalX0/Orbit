//! The execution engine: owns persistence, in-memory projections of active
//! executions, the worker registry and task dispatch.
//!
//! Every state change goes through [`Engine::commit`], which assigns sequence
//! numbers, persists the events transactionally and only then updates memory.

use crate::decide::{decide, invocation};
use crate::metrics::Metrics;
use crate::model::*;
use crate::store::{ExecutionSummary, PutDefinition, Store, WorkerRecord};
use orbit_ir::{WorkflowIr, WORKFLOW_CONTRACT_PREFIX};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

pub type Clock = Box<dyn Fn() -> u64 + Send>;

pub fn system_clock() -> Clock {
    Box::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    })
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// A worker that has not polled or heartbeated for this long is considered lost.
    pub session_timeout_ms: u64,
    pub heartbeat_interval_ms: u64,
    /// Execute `orbit.http.request` steps inside the runtime.
    pub http_adapter: bool,
    /// Resource limits (all optional; `None` means unlimited).
    pub limits: Limits,
}

/// Server-wide resource limits.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Limits {
    /// Running executions at once; further starts get 429 `RESOURCE_EXHAUSTED`.
    pub max_active_executions: Option<usize>,
    /// Elements one `for` step may fan out to; larger lists fail the step with
    /// `LIMIT_EXCEEDED`.
    pub max_fan_out: Option<usize>,
    /// History events per execution; past it the execution is failed with
    /// `LIMIT_EXCEEDED` (compensation and handlers still run).
    pub max_history_events: Option<u64>,
    /// Serialized size of an execution input, a task result or a signal
    /// payload; larger ones get 413 `PAYLOAD_TOO_LARGE`.
    pub max_payload_bytes: Option<usize>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            session_timeout_ms: 30_000,
            heartbeat_interval_ms: 5_000,
            http_adapter: false,
            limits: Limits::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EngineError {
    pub code: &'static str,
    pub message: String,
    #[serde(skip)]
    pub status: u16,
}

impl EngineError {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        EngineError {
            code,
            message: message.into(),
            status,
        }
    }
    fn not_found(what: &str) -> Self {
        Self::new(404, "NOT_FOUND", format!("{what} not found"))
    }
    fn storage(e: crate::store::StoreError) -> Self {
        Self::new(500, "STORAGE_ERROR", e.to_string())
    }
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractAnnouncement {
    pub task_name: String,
    pub contract_version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterWorker {
    pub worker_id: String,
    pub runtime: String,
    #[serde(default)]
    pub sdk: Option<String>,
    pub contracts: Vec<ContractAnnouncement>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Registration {
    pub accepted: bool,
    pub worker_session_id: String,
    pub heartbeat_interval_ms: u64,
    pub session_timeout_ms: u64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct Session {
    worker_id: String,
    runtime: String,
    contracts: HashSet<(String, String)>,
    last_seen_ms: u64,
    /// When `last_seen_ms` was last written to the worker registry.
    persisted_seen_ms: u64,
    inflight: HashSet<String>,
}

/// A task handed to a worker (RFC-0001 §8.3 `ExecuteTaskRequest`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskAssignment {
    pub token: String,
    pub execution_id: String,
    pub workflow: String,
    pub workflow_version: String,
    pub step_id: String,
    pub task_name: String,
    pub contract_version: String,
    pub input: Value,
    pub idempotency_key: String,
    pub attempt: u32,
    pub redelivery: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResultAck {
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeartbeatAck {
    pub accepted: bool,
    /// Tokens the worker should stop working on (cooperative cancellation).
    pub cancel: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerInfo {
    #[serde(flatten)]
    pub record: WorkerRecord,
    pub online: bool,
    pub sessions: usize,
    pub inflight: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StartResult {
    pub execution_id: String,
    pub created: bool,
}

const BUILTIN_SESSION_PREFIX: &str = "builtin:";
const CHILD_SESSION_PREFIX: &str = "child:";

/// Deterministic id of the child execution started by `unit` of `parent`, so
/// that starting it again after a crash is a no-op.
pub fn child_execution_id(parent: &str, unit: &str) -> String {
    let unit = unit
        .replace("compensate:", "compensate.")
        .replace('[', ".")
        .replace(']', "");
    format!("{parent}.{unit}")
}

pub struct Engine {
    store: Store,
    clock: Clock,
    pub config: EngineConfig,
    instance: String,
    started_at_ms: u64,
    by_version: BTreeMap<(String, String), Arc<WorkflowIr>>,
    by_hash: HashMap<String, Arc<WorkflowIr>>,
    active: BTreeMap<String, ExecutionState>,
    /// Units waiting for a worker, oldest first: `(scheduled_at_ms, execution,
    /// unit)`. Kept in step with `active` by `reindex`, so `poll` does not
    /// scan every execution.
    ready: BTreeSet<(u64, String, String)>,
    ready_by_execution: HashMap<String, Vec<(u64, String, String)>>,
    tokens: HashMap<String, (String, String)>,
    /// Which worker each task token was issued to (for per-worker authorization).
    token_workers: HashMap<String, String>,
    sessions: HashMap<String, Session>,
    /// Built-in tasks dispatched to the runtime itself, waiting to be run.
    builtin_jobs: Vec<TaskAssignment>,
    pub metrics: Metrics,
    pub changed: Arc<tokio::sync::Notify>,
    telemetry: Option<crate::telemetry::Exporter>,
    /// Shared mode (`--cluster-mode shared`): ownership, task queue and inbox
    /// shared with the other servers. `None` everywhere else.
    pub(crate) shared: Option<Arc<crate::shared::Ctx>>,
}

fn semver_key(v: &str) -> (Vec<u64>, String) {
    let core = v.split(['-', '+']).next().unwrap_or("");
    (
        core.split('.').map(|p| p.parse().unwrap_or(0)).collect(),
        v.to_string(),
    )
}

impl Engine {
    /// Open the engine and recover every non-terminal execution from history.
    pub fn open(store: Store, config: EngineConfig, clock: Clock) -> Result<Engine> {
        let now = clock();
        let mut engine = Engine {
            store,
            clock,
            config,
            instance: uuid::Uuid::new_v4().to_string(),
            started_at_ms: now,
            by_version: BTreeMap::new(),
            by_hash: HashMap::new(),
            active: BTreeMap::new(),
            ready: BTreeSet::new(),
            ready_by_execution: HashMap::new(),
            tokens: HashMap::new(),
            token_workers: HashMap::new(),
            sessions: HashMap::new(),
            builtin_jobs: vec![],
            metrics: Metrics::default(),
            changed: Arc::new(tokio::sync::Notify::new()),
            telemetry: None,
            shared: None,
        };
        for ir in engine.store.definitions().map_err(EngineError::storage)? {
            engine.cache_definition(ir);
        }
        engine.recover()?;
        Ok(engine)
    }

    pub fn now(&self) -> u64 {
        (self.clock)()
    }

    fn cache_definition(&mut self, ir: WorkflowIr) -> Arc<WorkflowIr> {
        let ir = Arc::new(ir);
        self.by_version
            .insert((ir.name.clone(), ir.version.clone()), ir.clone());
        self.by_hash.insert(ir.definition_hash.clone(), ir.clone());
        ir
    }

    /// RFC-0001 §9.9 crash recovery: load active executions, rebuild state
    /// from history, re-index in-flight work and resume. Attempts that were
    /// in flight are reconciled by `check_liveness` once their worker fails to
    /// report back within the session timeout (outcome treated as unknown,
    /// never as failed).
    fn recover(&mut self) -> Result<()> {
        let ids = self
            .store
            .active_execution_ids()
            .map_err(EngineError::storage)?;
        let mut in_flight = 0;
        for id in &ids {
            let history = self.store.history(id).map_err(EngineError::storage)?;
            for h in &history {
                if let Event::StepDispatched {
                    token, worker_id, ..
                } = &h.event
                {
                    self.token_workers.insert(token.clone(), worker_id.clone());
                }
            }
            let state = ExecutionState::replay(id, &history);
            for (key, u) in &state.units {
                for (t, _) in &u.tokens {
                    self.tokens.insert(t.clone(), (id.clone(), key.clone()));
                }
                if u.status() == UnitStatus::Dispatched {
                    in_flight += 1;
                }
            }
            self.active.insert(id.clone(), state);
            self.reindex(id);
        }
        if !ids.is_empty() {
            tracing::info!(
                executions = ids.len(),
                in_flight,
                "recovered active executions from history"
            );
        }
        for id in ids {
            self.advance(&id)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ definitions

    pub fn register_definition(&mut self, ir: WorkflowIr) -> Result<bool> {
        ir.validate()
            .map_err(|e| EngineError::new(400, "INVALID_IR", e.0))?;
        if ir.definition_hash.is_empty() {
            return Err(EngineError::new(
                400,
                "INVALID_IR",
                "definition_hash is required",
            ));
        }
        // A parent pins the exact child definition it was compiled against.
        for c in ir.contracts.values() {
            if let Some(hash) = &c.workflow_hash {
                if !self.by_hash.contains_key(hash) {
                    return Err(EngineError::new(
                        400,
                        "CHILD_NOT_REGISTERED",
                        format!(
                            "{}@{} starts child workflow {}@{} ({hash}), which is not registered; register the child first (`orbit deploy` does this automatically)",
                            ir.name,
                            ir.version,
                            c.name.trim_start_matches(WORKFLOW_CONTRACT_PREFIX),
                            c.version
                        ),
                    ));
                }
            }
        }
        let now = self.now();
        match self.store.put_definition(&ir, now).map_err(EngineError::storage)? {
            PutDefinition::Created => {
                tracing::info!(workflow = %ir.name, version = %ir.version, hash = %ir.definition_hash, "registered workflow definition");
                self.cache_definition(ir);
                Ok(true)
            }
            PutDefinition::Unchanged => Ok(false),
            PutDefinition::Conflict { existing_hash } => Err(EngineError::new(
                409,
                "DEFINITION_CONFLICT",
                format!(
                    "{}@{} is already registered with a different definition ({existing_hash}); workflow definitions are immutable, bump the version",
                    ir.name, ir.version
                ),
            )),
        }
    }

    pub fn definitions(&self) -> Vec<Arc<WorkflowIr>> {
        self.by_version.values().cloned().collect()
    }

    pub fn definition(&self, name: &str, version: Option<&str>) -> Option<Arc<WorkflowIr>> {
        match version {
            Some(v) => self
                .by_version
                .get(&(name.to_string(), v.to_string()))
                .cloned(),
            None => self
                .by_version
                .iter()
                .filter(|((n, _), _)| n == name)
                .max_by_key(|((_, v), _)| semver_key(v))
                .map(|(_, d)| d.clone()),
        }
    }

    // ------------------------------------------------------------- executions

    pub fn start(
        &mut self,
        workflow: &str,
        version: Option<&str>,
        input: Value,
        execution_id: Option<String>,
    ) -> Result<StartResult> {
        if self.shared.is_some() && self.definition(workflow, version).is_none() {
            // Registered on another server since this one loaded definitions.
            self.refresh_definitions()?;
        }
        let def = self.definition(workflow, version).ok_or_else(|| {
            EngineError::new(
                404,
                "WORKFLOW_NOT_FOUND",
                match version {
                    Some(v) => format!("workflow {workflow}@{v} is not registered"),
                    None => format!("workflow {workflow} is not registered"),
                },
            )
        })?;
        self.start_with(def, input, execution_id, None)
    }

    /// Start an execution of a specific definition. Child workflows pass their
    /// parent link; their input was already validated against the child's
    /// contract by the parent, so it is checked leniently.
    fn start_with(
        &mut self,
        def: Arc<WorkflowIr>,
        mut input: Value,
        execution_id: Option<String>,
        parent: Option<ParentRef>,
    ) -> Result<StartResult> {
        if input.is_null() {
            input = json!({});
        }
        def.input.apply_defaults(&mut input);
        self.check_payload(Some(&def), "execution input", &input)?;
        def.input.validate(&input, parent.is_none()).map_err(|e| {
            EngineError::new(
                400,
                "INVALID_INPUT",
                format!("workflow input is invalid at {e}"),
            )
        })?;
        let id = match execution_id {
            Some(id) => {
                if id.is_empty()
                    || id.len() > 200
                    || !id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))
                {
                    return Err(EngineError::new(
                        400,
                        "INVALID_EXECUTION_ID",
                        "execution ids may contain letters, digits and - _ . :",
                    ));
                }
                // Idempotent start: the same id never starts a second execution.
                if self.active.contains_key(&id)
                    || self
                        .store
                        .execution_exists(&id)
                        .map_err(EngineError::storage)?
                {
                    let existing = self.execution(&id)?;
                    if existing.definition_hash != def.definition_hash {
                        return Err(EngineError::new(
                            409,
                            "EXECUTION_EXISTS",
                            format!("execution `{id}` already exists for a different workflow"),
                        ));
                    }
                    return Ok(StartResult {
                        execution_id: id,
                        created: false,
                    });
                }
                id
            }
            None => uuid::Uuid::new_v4().to_string(),
        };
        if parent.is_none() {
            if let Some(max) = self.config.limits.max_active_executions {
                // Child workflows are part of their parent's work: only
                // executions started by callers count.
                let running = self.active.values().filter(|s| s.parent.is_none()).count();
                if running >= max {
                    return Err(EngineError::new(
                        429,
                        "RESOURCE_EXHAUSTED",
                        format!("{max} executions are already running (--max-active-executions)"),
                    ));
                }
            }
        }
        self.active.insert(id.clone(), ExecutionState::new(&id));
        let created = Event::WorkflowCreated {
            workflow: def.name.clone(),
            version: def.version.clone(),
            definition_hash: def.definition_hash.clone(),
            input,
            parent,
        };
        if let Err(e) = self.commit(&id, vec![created, Event::WorkflowStarted {}]) {
            self.active.remove(&id);
            return Err(e);
        }
        tracing::info!(execution_id = %id, workflow = %def.name, version = %def.version, "execution started");
        self.advance(&id)?;
        Ok(StartResult {
            execution_id: id,
            created: true,
        })
    }

    pub fn cancel(&mut self, id: &str, reason: &str) -> Result<ExecutionState> {
        let state = self.execution(id)?;
        if state.status.is_terminal() {
            return Err(EngineError::new(
                409,
                "EXECUTION_FINISHED",
                format!("execution is already {}", state.status.as_str()),
            ));
        }
        if self.forwards(id) {
            self.forward_command(id, "cancel", json!({ "reason": reason }))?;
            return Ok(state);
        }
        if state.abort.is_none() {
            self.commit(
                id,
                vec![Event::CancelRequested {
                    reason: reason.to_string(),
                }],
            )?;
            self.advance(id)?;
        }
        self.execution(id)
    }

    /// Send a typed signal to a running execution. The payload is validated
    /// against the workflow's `signal` declaration (defaults applied) and
    /// buffered until an `await` step takes it. Returns the history sequence
    /// number of the recorded signal.
    pub fn signal(&mut self, id: &str, name: &str, mut payload: Value) -> Result<u64> {
        let state = self.execution(id)?;
        if state.status.is_terminal() {
            return Err(EngineError::new(
                409,
                "EXECUTION_FINISHED",
                format!("execution is already {}", state.status.as_str()),
            ));
        }
        let def = self
            .by_hash
            .get(&state.definition_hash)
            .cloned()
            .ok_or_else(|| EngineError::not_found("workflow definition"))?;
        let Some(ty) = def.signals.get(name) else {
            let declared: Vec<&str> = def.signals.keys().map(String::as_str).collect();
            return Err(EngineError::new(
                404,
                "UNKNOWN_SIGNAL",
                if declared.is_empty() {
                    format!("{}@{} declares no signals", def.name, def.version)
                } else {
                    format!(
                        "{}@{} has no signal `{name}`; declared: {}",
                        def.name,
                        def.version,
                        declared.join(", ")
                    )
                },
            ));
        };
        self.check_payload(Some(&def), "signal payload", &payload)?;
        ty.apply_defaults(&mut payload);
        if let Err(e) = ty.validate(&payload, false) {
            return Err(EngineError::new(
                422,
                "SIGNAL_PAYLOAD_INVALID",
                format!("payload for signal `{name}` is invalid at {e}"),
            ));
        }
        if self.forwards(id) {
            self.forward_command(id, "signal", json!({ "name": name, "payload": payload }))?;
            return Ok(0);
        }
        let seq = state.last_seq + 1;
        self.commit(
            id,
            vec![Event::SignalReceived {
                name: name.to_string(),
                payload,
            }],
        )?;
        tracing::info!(execution_id = %id, signal = %name, "signal received");
        self.advance(id)?;
        Ok(seq)
    }

    /// Evaluate a declared query against the current state of an execution.
    /// Steps that have not completed read as `null`.
    pub fn query(&self, id: &str, name: &str) -> Result<Value> {
        let state = self.execution(id)?;
        let def = self
            .by_hash
            .get(&state.definition_hash)
            .cloned()
            .ok_or_else(|| EngineError::not_found("workflow definition"))?;
        let Some(q) = def.queries.get(name) else {
            let declared: Vec<&str> = def.queries.keys().map(String::as_str).collect();
            return Err(EngineError::new(
                404,
                "UNKNOWN_QUERY",
                if declared.is_empty() {
                    format!("{}@{} declares no queries", def.name, def.version)
                } else {
                    format!(
                        "{}@{} has no query `{name}`; declared: {}",
                        def.name,
                        def.version,
                        declared.join(", ")
                    )
                },
            ));
        };
        struct QueryScope<'a>(crate::decide::StateScope<'a>, Value);
        impl orbit_ir::Scope for QueryScope<'_> {
            fn input(&self) -> &Value {
                self.0.input()
            }
            fn step_output(&self, step: &str) -> Option<&Value> {
                self.0.step_output(step).or(Some(&self.1))
            }
            fn execution_field(&self, field: &str) -> Option<Value> {
                self.0.execution_field(field)
            }
        }
        let scope = QueryScope(crate::decide::StateScope::new(&state), Value::Null);
        orbit_ir::eval(&q.expr, &scope).map_err(|e| {
            EngineError::new(
                422,
                "QUERY_FAILED",
                format!("evaluating query `{name}`: {e}"),
            )
        })
    }

    /// Refresh the `ready` index for one execution after its state changed
    /// (or it left `active`).
    fn reindex(&mut self, id: &str) {
        if let Some(old) = self.ready_by_execution.remove(id) {
            for e in &old {
                self.ready.remove(e);
            }
        }
        let Some(state) = self.active.get(id) else {
            return;
        };
        let entries: Vec<(u64, String, String)> = state
            .units
            .iter()
            .filter(|(_, u)| u.status() == UnitStatus::Scheduled)
            .map(|(k, u)| (u.scheduled_at_ms.unwrap_or(0), id.to_string(), k.clone()))
            .collect();
        if !entries.is_empty() {
            self.ready.extend(entries.iter().cloned());
            self.ready_by_execution.insert(id.to_string(), entries);
        }
    }

    /// Reject a payload larger than `--max-payload-bytes` (or the workflow's
    /// own `max_payload_bytes`, whichever is smaller).
    fn check_payload(&self, def: Option<&WorkflowIr>, what: &str, value: &Value) -> Result<()> {
        let limits = match def {
            Some(d) => effective_limits(&self.config.limits, d),
            None => self.config.limits.clone(),
        };
        if let Some(max) = limits.max_payload_bytes {
            let size = serde_json::to_vec(value).map(|v| v.len()).unwrap_or(0);
            if size > max {
                return Err(EngineError::new(
                    413,
                    "PAYLOAD_TOO_LARGE",
                    format!("{what} is {size} bytes; the limit is {max} (--max-payload-bytes or the workflow's `limits`)"),
                ));
            }
        }
        Ok(())
    }

    pub fn execution(&self, id: &str) -> Result<ExecutionState> {
        if let Some(s) = self.active.get(id) {
            return Ok(s.clone());
        }
        let history = self.store.history(id).map_err(EngineError::storage)?;
        if history.is_empty() {
            return Err(EngineError::not_found("execution"));
        }
        Ok(ExecutionState::replay(id, &history))
    }

    pub fn history(&self, id: &str) -> Result<Vec<HistoryEvent>> {
        let h = self.store.history(id).map_err(EngineError::storage)?;
        if h.is_empty() {
            return Err(EngineError::not_found("execution"));
        }
        Ok(h)
    }

    pub fn list_executions(
        &self,
        status: Option<&str>,
        workflow: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ExecutionSummary>> {
        self.store
            .list_executions(status, workflow, limit.clamp(1, 1000))
            .map_err(EngineError::storage)
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    // ----------------------------------------------------------------- commit

    /// Persist events for one execution, then update the in-memory state.
    fn commit(&mut self, id: &str, events: Vec<Event>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let now = self.now();
        let mut state = self
            .active
            .get(id)
            .cloned()
            .ok_or_else(|| EngineError::not_found("active execution"))?;
        let mut history = Vec::with_capacity(events.len());
        for event in events {
            let h = HistoryEvent {
                seq: state.last_seq + 1,
                timestamp_ms: now,
                event,
            };
            apply(&mut state, &h);
            history.push(h);
        }
        match &self.shared {
            Some(ctx) => {
                let def = self.by_hash.get(&state.definition_hash).cloned();
                ctx.append(&mut self.store, &state, &history, def.as_deref())
            }
            None => self.store.append(&state, &history),
        }
        .map_err(EngineError::storage)?;
        // Export spans only once the events are durable.
        if let Some(t) = &self.telemetry {
            let ctx = self.trace_context(&state);
            t.export(crate::telemetry::spans_for(&state, &ctx, &history));
        }
        for h in &history {
            self.metrics.observe(&h.event);
            if let Event::StepDispatched {
                token,
                step,
                worker_id,
                ..
            } = &h.event
            {
                self.tokens
                    .insert(token.clone(), (id.to_string(), step.clone()));
                self.token_workers.insert(token.clone(), worker_id.clone());
            }
            tracing::debug!(execution_id = %id, seq = h.seq, event = h.event.name(), step = h.event.step().unwrap_or(""), "event");
            match &h.event {
                Event::WorkflowCompleted { .. } => {
                    tracing::info!(execution_id = %id, "execution completed")
                }
                Event::WorkflowFailed {
                    error,
                    compensation,
                } => {
                    tracing::warn!(execution_id = %id, code = %error.code, error = %error.message, compensation = ?compensation, "execution failed")
                }
                Event::WorkflowCancelled { .. } => {
                    tracing::info!(execution_id = %id, "execution cancelled")
                }
                Event::WorkflowTimedOut { .. } => {
                    tracing::warn!(execution_id = %id, "execution timed out")
                }
                Event::StepFailed { step, error } => {
                    tracing::warn!(execution_id = %id, step = %step, code = %error.code, error = %error.message, "step failed")
                }
                Event::RetryScheduled {
                    step,
                    next_attempt,
                    delay_ms,
                    ..
                } => {
                    tracing::info!(execution_id = %id, step = %step, attempt = next_attempt, delay_ms, "retry scheduled")
                }
                _ => {}
            }
        }
        if state.status.is_terminal() {
            for u in state.units.values() {
                for (t, _) in &u.tokens {
                    self.tokens.remove(t);
                    self.token_workers.remove(t);
                }
            }
            self.active.remove(id);
        } else {
            self.active.insert(id.to_string(), state);
        }
        self.reindex(id);
        self.changed.notify_waiters();
        Ok(())
    }

    /// Run the decider to a fixpoint and commit the resulting events as one batch.
    fn advance(&mut self, id: &str) -> Result<()> {
        let Some(state) = self.active.get(id) else {
            return Ok(());
        };
        let Some(def) = self.by_hash.get(&state.definition_hash).cloned() else {
            tracing::error!(execution_id = %id, hash = %state.definition_hash, "definition missing for execution");
            return Ok(());
        };
        let now = self.now();
        let mut scratch = state.clone();
        let mut batch = vec![];
        let limits = effective_limits(&self.config.limits, &def);
        for _ in 0..10_000 {
            let mut events = decide(&scratch, &def, now);
            if events.is_empty() {
                break;
            }
            enforce_limits(&limits, &scratch, &mut events);
            for e in events {
                let seq = scratch.last_seq + 1;
                apply(
                    &mut scratch,
                    &HistoryEvent {
                        seq,
                        timestamp_ms: now,
                        event: e.clone(),
                    },
                );
                batch.push(e);
            }
        }
        self.commit(id, batch)
    }

    /// Periodic work: timers, retries, deadlines, worker liveness, built-ins.
    pub fn tick(&mut self) -> Result<Vec<TaskAssignment>> {
        self.check_liveness()?;
        let ids: Vec<String> = self.active.keys().cloned().collect();
        for id in &ids {
            self.advance(id)?;
        }
        self.dispatch_builtins()?;
        self.dispatch_children()?;
        self.reconcile_children()?;
        Ok(std::mem::take(&mut self.builtin_jobs))
    }

    /// Dispatched attempts whose worker session is gone become uncertain and
    /// are redelivered with the same idempotency key (at-least-once delivery).
    fn check_liveness(&mut self) -> Result<()> {
        let now = self.now();
        let timeout = self.config.session_timeout_ms;
        self.sessions
            .retain(|_, s| now < s.last_seen_ms + timeout * 4);
        let mut lost: Vec<(String, String, u32, String)> = vec![];
        for (id, state) in &self.active {
            for (key, u) in &state.units {
                if u.status() != UnitStatus::Dispatched {
                    continue;
                }
                let session = u.worker_session.clone().unwrap_or_default();
                let alive = if session.starts_with(CHILD_SESSION_PREFIX) {
                    // Child workflows are durable executions, not processes.
                    true
                } else if let Some(inst) = session.strip_prefix(BUILTIN_SESSION_PREFIX) {
                    inst == self.instance
                } else {
                    let session_alive = self
                        .sessions
                        .get(&session)
                        .is_some_and(|s| now < s.last_seen_ms + timeout)
                        || self
                            .shared
                            .as_ref()
                            .is_some_and(|c| c.session_alive(&session, timeout));
                    // Shared mode: the server that handed the task out died
                    // and its result has not arrived.
                    session_alive
                        && !self
                            .shared
                            .as_ref()
                            .is_some_and(|c| c.claim_lost(u.token.as_deref()))
                };
                let since = u.dispatched_at_ms.unwrap_or(0).max(self.started_at_ms);
                if !alive && now >= since + timeout {
                    let reason = if session.starts_with(BUILTIN_SESSION_PREFIX) {
                        "runtime restarted while the built-in task was running".to_string()
                    } else {
                        format!(
                            "worker `{}` stopped responding",
                            u.worker_id.clone().unwrap_or_default()
                        )
                    };
                    lost.push((id.clone(), key.clone(), u.attempt, reason));
                }
            }
        }
        for (id, key, attempt, reason) in lost {
            tracing::warn!(execution_id = %id, step = %key, attempt, reason = %reason, "attempt outcome uncertain; redelivering");
            self.commit(
                &id,
                vec![Event::StepAttemptUncertain {
                    step: key,
                    attempt,
                    reason,
                }],
            )?;
            self.advance(&id)?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- workers

    pub fn register_worker(&mut self, req: RegisterWorker) -> Result<Registration> {
        if req.worker_id.trim().is_empty() {
            return Err(EngineError::new(
                400,
                "INVALID_REQUEST",
                "worker_id is required",
            ));
        }
        if req.contracts.is_empty() {
            return Err(EngineError::new(
                400,
                "INVALID_REQUEST",
                "a worker must announce at least one task contract",
            ));
        }
        let now = self.now();
        let mut warnings = vec![];
        for c in &req.contracts {
            if c.task_name.starts_with(WORKFLOW_CONTRACT_PREFIX) {
                return Err(EngineError::new(
                    400,
                    "RESERVED_TASK",
                    format!(
                        "`{}` names a child workflow, not a worker task",
                        c.task_name
                    ),
                ));
            }
            if c.task_name.starts_with("orbit.") {
                return Err(EngineError::new(
                    400,
                    "RESERVED_TASK",
                    format!("`{}` is a built-in task", c.task_name),
                ));
            }
            let mut versions: Vec<&str> = self
                .by_version
                .values()
                .filter_map(|d| d.contracts.get(&c.task_name).map(|k| k.version.as_str()))
                .collect();
            versions.sort();
            versions.dedup();
            if !versions.is_empty() && !versions.contains(&c.contract_version.as_str()) {
                warnings.push(format!(
                    "`{}` is announced at version {} but registered workflows use {}; this worker will not receive those tasks",
                    c.task_name,
                    c.contract_version,
                    versions.join(", ")
                ));
            }
        }
        let session_id = uuid::Uuid::new_v4().to_string();
        self.sessions.insert(
            session_id.clone(),
            Session {
                worker_id: req.worker_id.clone(),
                runtime: req.runtime.to_lowercase(),
                contracts: req
                    .contracts
                    .iter()
                    .map(|c| (c.task_name.clone(), c.contract_version.clone()))
                    .collect(),
                last_seen_ms: now,
                persisted_seen_ms: now,
                inflight: HashSet::new(),
            },
        );
        if let Some(ctx) = &self.shared {
            ctx.save_session(&session_id, &req.worker_id, &req.runtime, &req.contracts)?;
        }
        self.store
            .upsert_worker(&WorkerRecord {
                worker_id: req.worker_id.clone(),
                runtime: req.runtime.to_lowercase(),
                sdk: req.sdk.clone(),
                contracts: serde_json::to_value(&req.contracts).unwrap_or_default(),
                last_seen_ms: now,
                registered_at_ms: now,
            })
            .map_err(EngineError::storage)?;
        tracing::info!(worker_id = %req.worker_id, runtime = %req.runtime, tasks = req.contracts.len(), "worker registered");
        Ok(Registration {
            accepted: true,
            worker_session_id: session_id,
            heartbeat_interval_ms: self.config.heartbeat_interval_ms,
            session_timeout_ms: self.config.session_timeout_ms,
            warnings,
        })
    }

    fn touch(&mut self, session_id: &str) -> Result<()> {
        if self.shared.is_some() && !self.sessions.contains_key(session_id) {
            self.load_shared_session(session_id)?;
        }
        if let Some(ctx) = &self.shared {
            ctx.touched(session_id);
        }
        let now = self.now();
        let s = self.sessions.get_mut(session_id).ok_or_else(|| {
            EngineError::new(
                404,
                "UNKNOWN_SESSION",
                "unknown or expired worker session; register again",
            )
        })?;
        s.last_seen_ms = now;
        // The registry's `last_seen` is informational (`GET /v1/workers`);
        // writing it on every poll would cost a durable write per request.
        if now >= s.persisted_seen_ms + 1_000 {
            s.persisted_seen_ms = now;
            let wid = s.worker_id.clone();
            let _ = self.store.touch_worker(&wid, now);
        }
        Ok(())
    }

    pub fn heartbeat(&mut self, session_id: &str) -> Result<HeartbeatAck> {
        self.touch(session_id)?;
        let inflight: Vec<String> = self.sessions[session_id].inflight.iter().cloned().collect();
        let mut cancel = vec![];
        for token in inflight {
            let live = self.tokens.get(&token).and_then(|(id, key)| {
                let st = self.active.get(id)?;
                let u = st.units.get(key)?;
                Some(
                    u.status() == UnitStatus::Dispatched
                        && u.token.as_deref() == Some(token.as_str())
                        && (st.abort.is_none() || crate::model::is_cleanup_key(key)),
                )
            });
            if live != Some(true) {
                cancel.push(token.clone());
                self.sessions
                    .get_mut(session_id)
                    .unwrap()
                    .inflight
                    .remove(&token);
            }
        }
        if let Some(ctx) = &self.shared {
            cancel.extend(ctx.cancelled_for(session_id)?);
        }
        Ok(HeartbeatAck {
            accepted: true,
            cancel,
        })
    }

    pub fn unregister_worker(&mut self, session_id: &str) -> Result<()> {
        if self.shared.is_some() {
            if !self.sessions.contains_key(session_id) {
                self.load_shared_session(session_id)?;
            }
            self.release_shared_session(session_id)?;
        }
        let s = self
            .sessions
            .remove(session_id)
            .ok_or_else(|| EngineError::new(404, "UNKNOWN_SESSION", "unknown worker session"))?;
        tracing::info!(worker_id = %s.worker_id, "worker unregistered");
        // In-flight work of a departing worker is redelivered immediately.
        let mut lost = vec![];
        for token in s.inflight {
            if let Some((id, key)) = self.tokens.get(&token) {
                if let Some(u) = self.active.get(id).and_then(|st| st.units.get(key)) {
                    if u.status() == UnitStatus::Dispatched
                        && u.token.as_deref() == Some(token.as_str())
                    {
                        lost.push((id.clone(), key.clone(), u.attempt));
                    }
                }
            }
        }
        for (id, key, attempt) in lost {
            self.commit(
                &id,
                vec![Event::StepAttemptUncertain {
                    step: key,
                    attempt,
                    reason: "worker unregistered".into(),
                }],
            )?;
            self.advance(&id)?;
        }
        Ok(())
    }

    pub fn workers(&self) -> Result<Vec<WorkerInfo>> {
        let now = self.now();
        let records = self.store.workers().map_err(EngineError::storage)?;
        if let Some(ctx) = &self.shared {
            return ctx.workers(records, self.config.session_timeout_ms);
        }
        Ok(records
            .into_iter()
            .map(|record| {
                let live: Vec<&Session> = self
                    .sessions
                    .values()
                    .filter(|s| {
                        s.worker_id == record.worker_id
                            && now < s.last_seen_ms + self.config.session_timeout_ms
                    })
                    .collect();
                WorkerInfo {
                    online: !live.is_empty(),
                    sessions: live.len(),
                    inflight: live.iter().map(|s| s.inflight.len()).sum(),
                    record,
                }
            })
            .collect())
    }

    /// Hand the oldest matching queued task to a worker, if any.
    pub fn poll(&mut self, session_id: &str) -> Result<Option<TaskAssignment>> {
        self.touch(session_id)?;
        if self.shared.is_some() {
            return self.shared_poll(session_id);
        }
        let session = self.sessions[session_id].clone();
        // The oldest scheduled unit this session can serve.
        let mut best: Option<(u64, String, String)> = None;
        for (at, id, key) in &self.ready {
            let Some(state) = self.active.get(id) else {
                continue;
            };
            let Some(def) = self.by_hash.get(&state.definition_hash) else {
                continue;
            };
            let Some(inv) = invocation(def, key) else {
                continue;
            };
            if !session
                .contracts
                .contains(&(inv.task.clone(), inv.contract_version.clone()))
            {
                continue;
            }
            if let Some(lang) = &inv.language {
                if !lang.eq_ignore_ascii_case(&session.runtime) && lang != "any" {
                    continue;
                }
            }
            best = Some((*at, id.clone(), key.clone()));
            break;
        }
        let Some((_, id, key)) = best else {
            return Ok(None);
        };
        let assignment = self.dispatch(&id, &key, session_id, &session.worker_id)?;
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.inflight.insert(assignment.token.clone());
        }
        Ok(Some(assignment))
    }

    fn dispatch(
        &mut self,
        id: &str,
        key: &str,
        session_id: &str,
        worker_id: &str,
    ) -> Result<TaskAssignment> {
        let now = self.now();
        let state = &self.active[id];
        let def = self.by_hash[&state.definition_hash].clone();
        let inv = invocation(&def, key)
            .expect("unit has an invocation")
            .clone();
        let u = state.units[key].clone();
        let token = uuid::Uuid::new_v4().to_string();
        let deadline = inv.timeout_ms.map(|t| now + t);
        let assignment = TaskAssignment {
            token: token.clone(),
            execution_id: id.to_string(),
            workflow: def.name.clone(),
            workflow_version: def.version.clone(),
            step_id: key.to_string(),
            task_name: inv.task.clone(),
            contract_version: inv.contract_version.clone(),
            input: u.input.clone().unwrap_or(Value::Null),
            idempotency_key: u.idempotency_key.clone().unwrap_or_default(),
            attempt: u.attempt,
            redelivery: u.redelivery,
            deadline_ms: deadline,
            timeout_ms: inv.timeout_ms,
        };
        self.commit(
            id,
            vec![Event::StepDispatched {
                step: key.to_string(),
                attempt: u.attempt,
                token,
                worker_id: worker_id.to_string(),
                worker_session: session_id.to_string(),
                deadline_ms: deadline,
            }],
        )?;
        Ok(assignment)
    }

    /// Start child executions for scheduled `workflow` steps.
    fn dispatch_children(&mut self) -> Result<()> {
        let mut todo = vec![];
        for (id, state) in &self.active {
            let Some(def) = self.by_hash.get(&state.definition_hash) else {
                continue;
            };
            for (key, u) in &state.units {
                if u.status() != UnitStatus::Scheduled {
                    continue;
                }
                if let Some(inv) = invocation(def, key) {
                    if let Some(hash) = def
                        .contracts
                        .get(&inv.task)
                        .and_then(|c| c.workflow_hash.clone())
                    {
                        todo.push((
                            id.clone(),
                            key.clone(),
                            u.attempt,
                            hash,
                            u.input.clone().unwrap_or(Value::Null),
                        ));
                    }
                }
            }
        }
        for (id, key, attempt, hash, input) in todo {
            let child_id = child_execution_id(&id, &key);
            let failure = |code: &'static str, message: String| Event::StepAttemptFailed {
                step: key.clone(),
                attempt,
                error: TaskError::new(code, message, false),
            };
            let Some(child_def) = self.by_hash.get(&hash).cloned() else {
                self.commit(
                    &id,
                    vec![failure(
                        "CHILD_NOT_REGISTERED",
                        format!("child definition {hash} is not registered"),
                    )],
                )?;
                self.advance(&id)?;
                continue;
            };
            let parent = ParentRef {
                execution_id: id.clone(),
                step: key.clone(),
            };
            if let Err(e) = self.start_with(child_def, input, Some(child_id.clone()), Some(parent))
            {
                self.commit(&id, vec![failure("CHILD_START_FAILED", e.message)])?;
                self.advance(&id)?;
                continue;
            }
            if self
                .active
                .get(&id)
                .and_then(|s| s.units.get(&key))
                .map(|u| u.status())
                == Some(UnitStatus::Scheduled)
            {
                self.dispatch(
                    &id,
                    &key,
                    &format!("{CHILD_SESSION_PREFIX}{child_id}"),
                    "orbit-runtime",
                )?;
            }
        }
        Ok(())
    }

    /// Deliver finished children's results to their parents, and cancel
    /// children whose parent is aborting or no longer waits for them. Runs
    /// every tick, so it also repairs anything a crash interrupted.
    fn reconcile_children(&mut self) -> Result<()> {
        let mut finished = vec![];
        let mut cancel = vec![];
        for state in self.active.values() {
            for u in state.units.values() {
                let Some(child) = u
                    .worker_session
                    .as_deref()
                    .and_then(|s| s.strip_prefix(CHILD_SESSION_PREFIX))
                else {
                    continue;
                };
                match self.active.get(child) {
                    Some(c) => {
                        let parent_gone = u.status() != UnitStatus::Dispatched;
                        if c.abort.is_none() && (state.abort.is_some() || parent_gone) {
                            cancel.push(child.to_string());
                        }
                    }
                    None if u.status() == UnitStatus::Dispatched => {
                        finished.push((child.to_string(), u.token.clone().unwrap_or_default()));
                    }
                    None => {}
                }
            }
        }
        for child in cancel {
            tracing::info!(execution_id = %child, "cancelling child workflow because its parent no longer needs it");
            self.cancel(&child, "parent workflow aborted")?;
        }
        for (child, token) in finished {
            let state = match self.execution(&child) {
                Ok(s) => s,
                Err(_) => {
                    self.fail(
                        &token,
                        TaskError::new(
                            "CHILD_MISSING",
                            format!("child execution `{child}` not found"),
                            false,
                        ),
                    )?;
                    continue;
                }
            };
            let name = state.workflow.clone();
            match state.status {
                WorkflowStatus::Completed => {
                    self.complete(&token, state.output.unwrap_or(Value::Null))?;
                }
                WorkflowStatus::Failed => {
                    let e = state
                        .error
                        .unwrap_or_else(|| TaskError::new("UNKNOWN", "failed", false));
                    let mut err = TaskError::new(
                        &e.code,
                        format!("child workflow `{name}` failed: {}", e.message),
                        false,
                    );
                    err.details = Some(json!({ "child_execution_id": child }));
                    self.fail(&token, err)?;
                }
                WorkflowStatus::Cancelled => {
                    self.fail(
                        &token,
                        TaskError::new(
                            "CHILD_CANCELLED",
                            format!("child workflow `{name}` was cancelled"),
                            false,
                        ),
                    )?;
                }
                WorkflowStatus::TimedOut => {
                    self.fail(
                        &token,
                        TaskError::new(
                            "CHILD_TIMED_OUT",
                            format!("child workflow `{name}` timed out"),
                            false,
                        ),
                    )?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn dispatch_builtins(&mut self) -> Result<()> {
        let mut todo = vec![];
        for (id, state) in &self.active {
            let Some(def) = self.by_hash.get(&state.definition_hash) else {
                continue;
            };
            for (key, u) in &state.units {
                if u.status() != UnitStatus::Scheduled {
                    continue;
                }
                if let Some(inv) = invocation(def, key) {
                    if def.contracts.get(&inv.task).is_some_and(|c| c.builtin) {
                        todo.push((id.clone(), key.clone(), u.attempt));
                    }
                }
            }
        }
        for (id, key, attempt) in todo {
            if !self.config.http_adapter {
                self.commit(
                    &id,
                    vec![Event::StepAttemptFailed {
                        step: key,
                        attempt,
                        error: TaskError::new(codes::ADAPTER_DISABLED, "the HTTP adapter is disabled; start the server with --enable-http-adapter", false),
                    }],
                )?;
                self.advance(&id)?;
                continue;
            }
            let session = format!("{BUILTIN_SESSION_PREFIX}{}", self.instance);
            let job = self.dispatch(&id, &key, &session, "orbit-runtime")?;
            self.builtin_jobs.push(job);
        }
        Ok(())
    }

    /// Record a successful task result.
    pub fn complete(&mut self, token: &str, mut output: Value) -> Result<ResultAck> {
        if self.forwards_result(token) {
            // The owner holds the definition; the server-wide limit applies here.
            self.check_payload(None, "task result", &output)?;
            return self.forward_result(
                token,
                "complete",
                json!({ "token": token, "output": output }),
            );
        }
        let (id, key, unit, def) = match self.resolve_token(token)? {
            Ok(x) => x,
            Err(ack) => return Ok(ack),
        };
        let attempt = unit
            .tokens
            .iter()
            .find(|(t, _)| t == token)
            .map(|(_, a)| *a)
            .unwrap_or(unit.attempt);
        if output.is_null() {
            output = json!({});
        }
        self.check_payload(Some(&def), "task result", &output)?;
        let inv = invocation(&def, &key).cloned();
        let validation = inv
            .as_ref()
            .and_then(|i| def.contracts.get(&i.task))
            .map(|c| {
                c.output.apply_defaults(&mut output);
                c.output.validate(&output, false)
            })
            .unwrap_or(Ok(()));
        let event = match validation {
            Ok(()) => Event::StepSucceeded {
                step: key.clone(),
                attempt,
                output,
            },
            Err(e) => Event::StepAttemptFailed {
                step: key.clone(),
                attempt: unit.attempt,
                error: TaskError::new(
                    codes::OUTPUT_CONTRACT_VIOLATION,
                    format!(
                        "worker output for `{}` does not match its contract at {e}",
                        inv.map(|i| i.task).unwrap_or_default()
                    ),
                    false,
                ),
            },
        };
        let accepted = matches!(event, Event::StepSucceeded { .. });
        self.commit(&id, vec![event])?;
        self.advance(&id)?;
        Ok(ResultAck {
            accepted,
            reason: (!accepted).then(|| "output violates the task contract".into()),
        })
    }

    /// Record a failed attempt.
    pub fn fail(&mut self, token: &str, error: TaskError) -> Result<ResultAck> {
        if self.forwards_result(token) {
            return self.forward_result(token, "fail", json!({ "token": token, "error": error }));
        }
        let (id, key, unit, _) = match self.resolve_token(token)? {
            Ok(x) => x,
            Err(ack) => return Ok(ack),
        };
        // Failures only count for the attempt currently in flight.
        if unit.token.as_deref() != Some(token) {
            self.commit(
                &id,
                vec![Event::StaleResultIgnored {
                    step: key,
                    token: token.into(),
                    outcome: "failure".into(),
                }],
            )?;
            return Ok(ResultAck {
                accepted: false,
                reason: Some("stale attempt".into()),
            });
        }
        self.commit(
            &id,
            vec![Event::StepAttemptFailed {
                step: key,
                attempt: unit.attempt,
                error,
            }],
        )?;
        self.advance(&id)?;
        Ok(ResultAck {
            accepted: true,
            reason: None,
        })
    }

    #[allow(clippy::type_complexity)]
    fn resolve_token(
        &mut self,
        token: &str,
    ) -> Result<std::result::Result<(String, String, UnitState, Arc<WorkflowIr>), ResultAck>> {
        let Some((id, key)) = self.tokens.get(token).cloned() else {
            return Ok(Err(ResultAck {
                accepted: false,
                reason: Some("unknown token (execution finished or token invalid)".into()),
            }));
        };
        if let Some(ctx) = &self.shared {
            ctx.stage_done(&id, token);
        }
        let state = &self.active[&id];
        let unit = state.units.get(&key).cloned().unwrap_or_default();
        let def = self.by_hash[&state.definition_hash].clone();
        if let Some(session) = &unit.worker_session {
            if let Some(s) = self.sessions.get_mut(session) {
                s.inflight.remove(token);
                s.last_seen_ms = (self.clock)();
            }
        }
        // A result for any attempt of an unresolved unit is accepted: the work
        // really happened (reconciliation of uncertain/timed-out attempts).
        let unresolved = matches!(
            unit.status(),
            UnitStatus::Dispatched
                | UnitStatus::Scheduled
                | UnitStatus::RetryWait
                | UnitStatus::Uncertain
                | UnitStatus::AttemptFailed
        );
        if !unresolved {
            self.commit(
                &id,
                vec![Event::StaleResultIgnored {
                    step: key,
                    token: token.into(),
                    outcome: "duplicate".into(),
                }],
            )?;
            return Ok(Err(ResultAck {
                accepted: false,
                reason: Some("duplicate result; step already resolved".into()),
            }));
        }
        Ok(Ok((id, key, unit, def)))
    }

    pub fn store_stats(&self) -> Result<Vec<(String, u64)>> {
        self.store.count_by_status().map_err(EngineError::storage)
    }

    /// Export OpenTelemetry traces of executions (see [`crate::telemetry`]).
    pub fn enable_telemetry(&mut self, config: crate::telemetry::TelemetryConfig) {
        tracing::info!(endpoint = %config.endpoint, service = %config.service_name, "exporting OpenTelemetry traces");
        self.telemetry = Some(crate::telemetry::Exporter::start(config));
    }

    /// Trace of an execution: its own, or (for a child workflow) the trace of
    /// its top-level ancestor, nested under the parent step's span.
    fn trace_context(&self, state: &ExecutionState) -> crate::telemetry::TraceContext {
        use crate::telemetry::{trace_id, unit_span_id, TraceContext};
        let Some(parent) = &state.parent else {
            return TraceContext {
                trace_id: trace_id(&state.id),
                parent_span_id: None,
            };
        };
        let mut root = parent.execution_id.clone();
        for _ in 0..64 {
            let next = match self.active.get(&root) {
                Some(s) => s.parent.clone(),
                None => self.execution(&root).ok().and_then(|s| s.parent),
            };
            match next {
                Some(p) => root = p.execution_id,
                None => break,
            }
        }
        TraceContext {
            trace_id: trace_id(&root),
            // Child workflow steps are dispatched exactly once.
            parent_span_id: Some(unit_span_id(&parent.execution_id, &parent.step, 1)),
        }
    }

    /// The worker id that owns a session, if the session exists.
    pub fn session_worker(&self, session_id: &str) -> Option<&str> {
        self.sessions.get(session_id).map(|s| s.worker_id.as_str())
    }

    /// The worker id a task token was issued to, if the token is known.
    pub fn token_worker(&self, token: &str) -> Option<&str> {
        self.token_workers.get(token).map(String::as_str)
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Load definitions registered in the store since this engine opened
    /// (another server registered them). Used by cluster standbys, whose
    /// engine only reads. Returns how many were new.
    pub fn refresh_definitions(&mut self) -> Result<usize> {
        let mut added = 0;
        for ir in self.store.definitions().map_err(EngineError::storage)? {
            if !self.by_hash.contains_key(&ir.definition_hash) {
                self.cache_definition(ir);
                added += 1;
            }
        }
        Ok(added)
    }
}

/// The stricter of the server's limits and the workflow's `limits { ... }`.
pub fn effective_limits(server: &Limits, def: &WorkflowIr) -> Limits {
    fn min<T: Ord + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
        match (a, b) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, None) => x,
            (None, y) => y,
        }
    }
    let w = def.limits.clone().unwrap_or_default();
    Limits {
        max_active_executions: server.max_active_executions,
        max_fan_out: min(server.max_fan_out, w.max_fan_out.map(|v| v as usize)),
        max_history_events: min(server.max_history_events, w.max_history_events),
        max_payload_bytes: min(
            server.max_payload_bytes,
            w.max_payload_bytes.map(|v| v as usize),
        ),
    }
}

/// Apply fan-out and history limits to the decider's next events (the decider
/// itself stays a pure function of the history and the definition).
fn enforce_limits(limits: &Limits, state: &ExecutionState, events: &mut Vec<Event>) {
    if let Some(max) = limits.max_fan_out {
        for e in events.iter_mut() {
            if let Event::StepFanOut { step, items } = e {
                if items.len() > max {
                    *e = Event::StepFailed {
                        step: step.clone(),
                        error: TaskError::new(
                            "LIMIT_EXCEEDED",
                            format!(
                                "`for` over {} elements exceeds the fan-out limit of {max} (--max-fan-out or the workflow's `limits`)",
                                items.len()
                            ),
                            false,
                        ),
                    };
                }
            }
        }
    }
    if let Some(max) = limits.max_history_events {
        let would_be = state.last_seq + events.len() as u64;
        if state.abort.is_none() && would_be > max && !state.status.is_terminal() {
            // Stop scheduling new work; compensation and handlers still run.
            events.clear();
            events.push(Event::LimitExceeded {
                limit: "max_history_events".into(),
                message: format!(
                    "the execution reached {max} history events (--max-history-events or the workflow's `limits`)"
                ),
            });
        }
    }
}

// ------------------------------------------------------------ shared mode
//
// Engine operations used by `crate::shared` (`--cluster-mode shared`). In
// that mode this engine holds only the executions this server owns; the
// methods below load and drop them, record dispatches made by other servers,
// and route commands for executions owned elsewhere to their owner's inbox.

impl Engine {
    /// The shared-mode context, when this engine runs in shared mode.
    pub fn shared_context(&self) -> Option<Arc<crate::shared::Ctx>> {
        self.shared.clone()
    }

    /// True when this engine holds (owns) the execution.
    pub(crate) fn owns(&self, id: &str) -> bool {
        self.active.contains_key(id)
    }

    /// A command for `id` must be sent to the server that owns it.
    fn forwards(&self, id: &str) -> bool {
        self.shared.as_ref().is_some_and(|c| !c.applying_inbox()) && !self.active.contains_key(id)
    }

    /// A result for `token` must be sent to the owner of its execution (the
    /// token was issued by another server, or its dispatch is not recorded yet).
    fn forwards_result(&self, token: &str) -> bool {
        self.shared.as_ref().is_some_and(|c| !c.applying_inbox())
            && !self.tokens.contains_key(token)
    }

    fn forward_command(&mut self, id: &str, kind: &str, payload: Value) -> Result<()> {
        let ctx = self.shared.clone().expect("shared mode");
        match ctx.forward(id, kind, payload, None)? {
            Some(seq) => {
                crate::shared::note_forwarded(seq);
                Ok(())
            }
            None => Err(EngineError::new(
                409,
                "EXECUTION_FINISHED",
                "execution has already finished",
            )),
        }
    }

    fn forward_result(&mut self, token: &str, kind: &str, payload: Value) -> Result<ResultAck> {
        let unknown = ResultAck {
            accepted: false,
            reason: Some("unknown token (execution finished or token invalid)".into()),
        };
        let Some(exec) = crate::shared::token_execution(token) else {
            return Ok(unknown);
        };
        let ctx = self.shared.clone().expect("shared mode");
        Ok(match ctx.forward(exec, kind, payload, Some(token))? {
            Some(_) => ResultAck {
                accepted: true,
                reason: None,
            },
            None => unknown,
        })
    }

    /// Claim a task from the shared queue for a worker session.
    fn shared_poll(&mut self, session_id: &str) -> Result<Option<TaskAssignment>> {
        let ctx = self.shared.clone().expect("shared mode");
        if !ctx.should_claim(session_id) {
            return Ok(None);
        }
        let s = self.sessions[session_id].clone();
        for _ in 0..16 {
            let now = self.now();
            match ctx.claim(session_id, &s.worker_id, &s.runtime, &s.contracts, now)? {
                crate::shared::Claimed::Nothing => return Ok(None),
                crate::shared::Claimed::Stale => continue,
                crate::shared::Claimed::Remote(a) => return Ok(Some(a)),
                crate::shared::Claimed::Local {
                    row_id,
                    offer,
                    assignment: a,
                } => {
                    match self.apply_dispatch(
                        &a.execution_id,
                        &a.step_id,
                        a.attempt,
                        offer,
                        &a.token,
                        &s.worker_id,
                        session_id,
                        a.deadline_ms,
                    ) {
                        Ok(true) => return Ok(Some(a)),
                        Ok(false) => {
                            ctx.retire(row_id, &a.token)?;
                            continue;
                        }
                        Err(e) => {
                            let _ = ctx.release(row_id, &a.token);
                            return Err(e);
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    /// Shared-mode poll, first half: touch the session and return what a
    /// claim needs, or `None` when the queue has not changed since this
    /// session last found it empty.
    #[allow(clippy::type_complexity)]
    pub(crate) fn prepare_claim(
        &mut self,
        session_id: &str,
    ) -> Result<Option<(String, String, HashSet<(String, String)>)>> {
        self.touch(session_id)?;
        let ctx = self.shared.clone().expect("shared mode");
        if !ctx.should_claim(session_id) {
            return Ok(None);
        }
        let s = &self.sessions[session_id];
        Ok(Some((
            s.worker_id.clone(),
            s.runtime.clone(),
            s.contracts.clone(),
        )))
    }

    /// Record a dispatch made from the shared queue, if the unit is still
    /// waiting for exactly that offer. Returns false for a stale claim.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_dispatch(
        &mut self,
        id: &str,
        key: &str,
        attempt: u32,
        offer: u32,
        token: &str,
        worker_id: &str,
        session: &str,
        deadline_ms: Option<u64>,
    ) -> Result<bool> {
        let current = self
            .active
            .get(id)
            .and_then(|s| s.units.get(key))
            .is_some_and(|u| {
                u.status() == UnitStatus::Scheduled
                    && u.attempt == attempt
                    && u.dispatch_count == offer
            });
        if !current {
            return Ok(false);
        }
        self.commit(
            id,
            vec![Event::StepDispatched {
                step: key.to_string(),
                attempt,
                token: token.to_string(),
                worker_id: worker_id.to_string(),
                worker_session: session.to_string(),
                deadline_ms,
            }],
        )?;
        Ok(true)
    }

    /// The attempt holding `token` lost its worker (it unregistered on
    /// another server): its outcome is uncertain and it is redelivered.
    pub(crate) fn mark_uncertain(&mut self, token: &str, reason: &str) -> Result<bool> {
        let Some((id, key)) = self.tokens.get(token).cloned() else {
            return Ok(false);
        };
        let Some(u) = self.active.get(&id).and_then(|s| s.units.get(&key)) else {
            return Ok(false);
        };
        if u.status() != UnitStatus::Dispatched || u.token.as_deref() != Some(token) {
            return Ok(false);
        }
        let attempt = u.attempt;
        tracing::warn!(execution_id = %id, step = %key, attempt, reason = %reason, "attempt outcome uncertain; redelivering");
        self.commit(
            &id,
            vec![Event::StepAttemptUncertain {
                step: key,
                attempt,
                reason: reason.to_string(),
            }],
        )?;
        self.advance(&id)?;
        Ok(true)
    }

    /// Load a worker session registered on another server.
    fn load_shared_session(&mut self, session_id: &str) -> Result<()> {
        let ctx = self.shared.clone().expect("shared mode");
        if let Some((worker_id, runtime, contracts)) =
            ctx.load_session(session_id, self.config.session_timeout_ms)?
        {
            let now = self.now();
            self.sessions.insert(
                session_id.to_string(),
                Session {
                    worker_id,
                    runtime,
                    contracts: contracts.into_iter().collect(),
                    last_seen_ms: now,
                    persisted_seen_ms: now,
                    inflight: HashSet::new(),
                },
            );
        }
        Ok(())
    }

    /// Unregistration in shared mode: delete the session everywhere and
    /// redeliver what it held, wherever those executions are owned.
    fn release_shared_session(&mut self, session_id: &str) -> Result<()> {
        let ctx = self.shared.clone().expect("shared mode");
        for (exec, token) in ctx.drop_session(session_id)? {
            if self.tokens.contains_key(&token) {
                self.mark_uncertain(&token, "worker unregistered")?;
            } else {
                ctx.forward(
                    &exec,
                    "uncertain",
                    json!({ "token": token, "reason": "worker unregistered" }),
                    None,
                )?;
            }
        }
        Ok(())
    }

    /// [`Engine::session_worker`] that also finds sessions opened on other
    /// servers (shared mode).
    pub fn session_worker_lookup(&mut self, session_id: &str) -> Option<String> {
        if self.shared.is_some() && !self.sessions.contains_key(session_id) {
            let _ = self.load_shared_session(session_id);
        }
        self.session_worker(session_id).map(str::to_string)
    }

    /// [`Engine::token_worker`] that also finds tokens issued by other
    /// servers (shared mode).
    pub fn token_worker_lookup(&self, token: &str) -> Option<String> {
        if let Some(w) = self.token_worker(token) {
            return Some(w.to_string());
        }
        self.shared.as_ref().and_then(|c| c.token_worker(token))
    }

    /// Take ownership of executions (startup or takeover): rebuild them from
    /// history like [`Engine::open`] does, put their queued tasks back on the
    /// shared queue, and resume them. Attempts in flight stay in flight while
    /// their worker sessions live; the others become uncertain and are
    /// redelivered by the liveness check.
    pub(crate) fn adopt(&mut self, items: &[(String, i64)], from: &str) -> Result<usize> {
        let ctx = self.shared.clone().expect("shared mode");
        let mut loaded = vec![];
        let mut in_flight = 0;
        for (id, epoch) in items {
            let history = self.store.history(id).map_err(EngineError::storage)?;
            if history.is_empty() {
                continue;
            }
            let state = ExecutionState::replay(id, &history);
            if state.status.is_terminal() {
                continue;
            }
            if !self.by_hash.contains_key(&state.definition_hash) {
                self.refresh_definitions()?;
            }
            for h in &history {
                if let Event::StepDispatched {
                    token, worker_id, ..
                } = &h.event
                {
                    self.token_workers.insert(token.clone(), worker_id.clone());
                }
            }
            for (key, u) in &state.units {
                for (t, _) in &u.tokens {
                    self.tokens.insert(t.clone(), (id.clone(), key.clone()));
                }
                if u.status() == UnitStatus::Dispatched {
                    in_flight += 1;
                }
            }
            ctx.set_epoch(id, *epoch);
            self.active.insert(id.clone(), state);
            self.reindex(id);
            loaded.push(id.clone());
        }
        let mut offers = vec![];
        for id in &loaded {
            let state = &self.active[id];
            let Some(def) = self.by_hash.get(&state.definition_hash) else {
                continue;
            };
            for (key, u) in &state.units {
                if u.status() == UnitStatus::Scheduled {
                    if let Some(o) = crate::shared::offer_for(state, key, def) {
                        offers.push((id.clone(), o));
                    }
                }
            }
        }
        ctx.republish(offers, from)?;
        if !loaded.is_empty() {
            tracing::info!(
                executions = loaded.len(),
                in_flight,
                from = %from,
                "shared: took ownership of executions"
            );
        }
        for id in &loaded {
            if let Err(e) = self.advance(id) {
                tracing::error!(execution_id = %id, error = %e, "shared: resuming an adopted execution failed");
            }
        }
        Ok(loaded.len())
    }

    /// Drop an execution this server no longer owns (another server took it
    /// over): it stops ticking it and forgets its tokens.
    pub(crate) fn evict(&mut self, id: &str) {
        if let Some(state) = self.active.remove(id) {
            for u in state.units.values() {
                for (t, _) in &u.tokens {
                    self.tokens.remove(t);
                    self.token_workers.remove(t);
                }
            }
            self.reindex(id);
            tracing::warn!(execution_id = %id, "shared: another server owns this execution now; dropped it");
        }
        if let Some(ctx) = &self.shared {
            ctx.forget(id);
        }
    }

    /// (session, token) of the worker attempts in flight in executions this
    /// engine owns.
    pub(crate) fn dispatched_attempts(&self) -> Vec<(String, String)> {
        let mut out = vec![];
        for state in self.active.values() {
            for u in state.units.values() {
                if u.status() != UnitStatus::Dispatched {
                    continue;
                }
                if let (Some(s), Some(t)) = (&u.worker_session, &u.token) {
                    if !s.starts_with(CHILD_SESSION_PREFIX)
                        && !s.starts_with(BUILTIN_SESSION_PREFIX)
                    {
                        out.push((s.clone(), t.clone()));
                    }
                }
            }
        }
        out
    }

    /// Forget sessions that no longer exist (unregistered or expired elsewhere).
    pub(crate) fn forget_sessions(&mut self, ids: &[String]) {
        for id in ids {
            self.sessions.remove(id);
        }
    }

    /// Ids of the executions this engine owns.
    pub(crate) fn owned_ids(&self) -> Vec<String> {
        self.active.keys().cloned().collect()
    }

    /// Queued worker units (execution, unit, attempt, offer) of owned executions.
    pub(crate) fn queued_units(&self) -> Vec<(String, String, i64, i64)> {
        let mut out = vec![];
        for (id, state) in &self.active {
            let Some(def) = self.by_hash.get(&state.definition_hash) else {
                continue;
            };
            for (key, u) in &state.units {
                if u.status() == UnitStatus::Scheduled
                    && crate::shared::worker_invocation(def, key).is_some()
                {
                    out.push((
                        id.clone(),
                        key.clone(),
                        u.attempt as i64,
                        u.dispatch_count as i64,
                    ));
                }
            }
        }
        out
    }
}
