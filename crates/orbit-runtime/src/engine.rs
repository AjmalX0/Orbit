//! The execution engine: owns persistence, in-memory projections of active
//! executions, the worker registry and task dispatch.
//!
//! Every state change goes through [`Engine::commit`], which assigns sequence
//! numbers, persists the events transactionally and only then updates memory.

use crate::decide::{decide, invocation};
use crate::metrics::Metrics;
use crate::model::*;
use crate::store::{ExecutionSummary, PutDefinition, Store, WorkerRecord};
use orbit_ir::WorkflowIr;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

pub type Clock = Box<dyn Fn() -> u64 + Send>;

pub fn system_clock() -> Clock {
    Box::new(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0))
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// A worker that has not polled or heartbeated for this long is considered lost.
    pub session_timeout_ms: u64,
    pub heartbeat_interval_ms: u64,
    /// Execute `orbit.http.request` steps inside the runtime.
    pub http_adapter: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig { session_timeout_ms: 30_000, heartbeat_interval_ms: 5_000, http_adapter: false }
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
        EngineError { code, message: message.into(), status }
    }
    fn not_found(what: &str) -> Self {
        Self::new(404, "NOT_FOUND", format!("{what} not found"))
    }
    fn storage(e: rusqlite::Error) -> Self {
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

pub struct Engine {
    store: Store,
    clock: Clock,
    pub config: EngineConfig,
    instance: String,
    started_at_ms: u64,
    by_version: BTreeMap<(String, String), Arc<WorkflowIr>>,
    by_hash: HashMap<String, Arc<WorkflowIr>>,
    active: BTreeMap<String, ExecutionState>,
    tokens: HashMap<String, (String, String)>,
    sessions: HashMap<String, Session>,
    /// Built-in tasks dispatched to the runtime itself, waiting to be run.
    builtin_jobs: Vec<TaskAssignment>,
    pub metrics: Metrics,
    pub changed: Arc<tokio::sync::Notify>,
}

fn semver_key(v: &str) -> (Vec<u64>, String) {
    let core = v.split(['-', '+']).next().unwrap_or("");
    (core.split('.').map(|p| p.parse().unwrap_or(0)).collect(), v.to_string())
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
            tokens: HashMap::new(),
            sessions: HashMap::new(),
            builtin_jobs: vec![],
            metrics: Metrics::default(),
            changed: Arc::new(tokio::sync::Notify::new()),
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
        self.by_version.insert((ir.name.clone(), ir.version.clone()), ir.clone());
        self.by_hash.insert(ir.definition_hash.clone(), ir.clone());
        ir
    }

    /// RFC-0001 §9.9 crash recovery: load active executions, rebuild state
    /// from history, re-index in-flight work and resume. Attempts that were
    /// in flight are reconciled by `check_liveness` once their worker fails to
    /// report back within the session timeout (outcome treated as unknown,
    /// never as failed).
    fn recover(&mut self) -> Result<()> {
        let ids = self.store.active_execution_ids().map_err(EngineError::storage)?;
        let mut in_flight = 0;
        for id in &ids {
            let history = self.store.history(id).map_err(EngineError::storage)?;
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
        }
        if !ids.is_empty() {
            tracing::info!(executions = ids.len(), in_flight, "recovered active executions from history");
        }
        for id in ids {
            self.advance(&id)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ definitions

    pub fn register_definition(&mut self, ir: WorkflowIr) -> Result<bool> {
        ir.validate().map_err(|e| EngineError::new(400, "INVALID_IR", e.0))?;
        if ir.definition_hash.is_empty() {
            return Err(EngineError::new(400, "INVALID_IR", "definition_hash is required"));
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
            Some(v) => self.by_version.get(&(name.to_string(), v.to_string())).cloned(),
            None => self
                .by_version
                .iter()
                .filter(|((n, _), _)| n == name)
                .max_by_key(|((_, v), _)| semver_key(v))
                .map(|(_, d)| d.clone()),
        }
    }

    // ------------------------------------------------------------- executions

    pub fn start(&mut self, workflow: &str, version: Option<&str>, mut input: Value, execution_id: Option<String>) -> Result<StartResult> {
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
        if input.is_null() {
            input = json!({});
        }
        def.input.apply_defaults(&mut input);
        def.input
            .validate(&input, true)
            .map_err(|e| EngineError::new(400, "INVALID_INPUT", format!("workflow input is invalid at {e}")))?;
        let id = match execution_id {
            Some(id) => {
                if id.is_empty() || id.len() > 200 || !id.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c)) {
                    return Err(EngineError::new(400, "INVALID_EXECUTION_ID", "execution ids may contain letters, digits and - _ . :"));
                }
                // Idempotent start: the same id never starts a second execution.
                if self.active.contains_key(&id) || self.store.execution_exists(&id).map_err(EngineError::storage)? {
                    let existing = self.execution(&id)?;
                    if existing.definition_hash != def.definition_hash {
                        return Err(EngineError::new(409, "EXECUTION_EXISTS", format!("execution `{id}` already exists for a different workflow")));
                    }
                    return Ok(StartResult { execution_id: id, created: false });
                }
                id
            }
            None => uuid::Uuid::new_v4().to_string(),
        };
        self.active.insert(id.clone(), ExecutionState::new(&id));
        let created = Event::WorkflowCreated {
            workflow: def.name.clone(),
            version: def.version.clone(),
            definition_hash: def.definition_hash.clone(),
            input,
        };
        if let Err(e) = self.commit(&id, vec![created, Event::WorkflowStarted {}]) {
            self.active.remove(&id);
            return Err(e);
        }
        tracing::info!(execution_id = %id, workflow = %def.name, version = %def.version, "execution started");
        self.advance(&id)?;
        Ok(StartResult { execution_id: id, created: true })
    }

    pub fn cancel(&mut self, id: &str, reason: &str) -> Result<ExecutionState> {
        let state = self.execution(id)?;
        if state.status.is_terminal() {
            return Err(EngineError::new(409, "EXECUTION_FINISHED", format!("execution is already {}", state.status.as_str())));
        }
        if state.abort.is_none() {
            self.commit(id, vec![Event::CancelRequested { reason: reason.to_string() }])?;
            self.advance(id)?;
        }
        self.execution(id)
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

    pub fn list_executions(&self, status: Option<&str>, workflow: Option<&str>, limit: u32) -> Result<Vec<ExecutionSummary>> {
        self.store.list_executions(status, workflow, limit.clamp(1, 1000)).map_err(EngineError::storage)
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
        let mut state = self.active.get(id).cloned().ok_or_else(|| EngineError::not_found("active execution"))?;
        let mut history = Vec::with_capacity(events.len());
        for event in events {
            let h = HistoryEvent { seq: state.last_seq + 1, timestamp_ms: now, event };
            apply(&mut state, &h);
            history.push(h);
        }
        self.store.append(&state, &history).map_err(EngineError::storage)?;
        for h in &history {
            self.metrics.observe(&h.event);
            if let Event::StepDispatched { token, step, .. } = &h.event {
                self.tokens.insert(token.clone(), (id.to_string(), step.clone()));
            }
            tracing::debug!(execution_id = %id, seq = h.seq, event = h.event.name(), step = h.event.step().unwrap_or(""), "event");
            match &h.event {
                Event::WorkflowCompleted { .. } => tracing::info!(execution_id = %id, "execution completed"),
                Event::WorkflowFailed { error, compensation } => {
                    tracing::warn!(execution_id = %id, code = %error.code, error = %error.message, compensation = ?compensation, "execution failed")
                }
                Event::WorkflowCancelled { .. } => tracing::info!(execution_id = %id, "execution cancelled"),
                Event::WorkflowTimedOut { .. } => tracing::warn!(execution_id = %id, "execution timed out"),
                Event::StepFailed { step, error } => tracing::warn!(execution_id = %id, step = %step, code = %error.code, error = %error.message, "step failed"),
                Event::RetryScheduled { step, next_attempt, delay_ms, .. } => {
                    tracing::info!(execution_id = %id, step = %step, attempt = next_attempt, delay_ms, "retry scheduled")
                }
                _ => {}
            }
        }
        if state.status.is_terminal() {
            for u in state.units.values() {
                for (t, _) in &u.tokens {
                    self.tokens.remove(t);
                }
            }
            self.active.remove(id);
        } else {
            self.active.insert(id.to_string(), state);
        }
        self.changed.notify_waiters();
        Ok(())
    }

    /// Run the decider to a fixpoint and commit the resulting events as one batch.
    fn advance(&mut self, id: &str) -> Result<()> {
        let Some(state) = self.active.get(id) else { return Ok(()) };
        let Some(def) = self.by_hash.get(&state.definition_hash).cloned() else {
            tracing::error!(execution_id = %id, hash = %state.definition_hash, "definition missing for execution");
            return Ok(());
        };
        let now = self.now();
        let mut scratch = state.clone();
        let mut batch = vec![];
        for _ in 0..10_000 {
            let events = decide(&scratch, &def, now);
            if events.is_empty() {
                break;
            }
            for e in events {
                let seq = scratch.last_seq + 1;
                apply(&mut scratch, &HistoryEvent { seq, timestamp_ms: now, event: e.clone() });
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
        Ok(std::mem::take(&mut self.builtin_jobs))
    }

    /// Dispatched attempts whose worker session is gone become uncertain and
    /// are redelivered with the same idempotency key (at-least-once delivery).
    fn check_liveness(&mut self) -> Result<()> {
        let now = self.now();
        let timeout = self.config.session_timeout_ms;
        self.sessions.retain(|_, s| now < s.last_seen_ms + timeout * 4);
        let mut lost: Vec<(String, String, u32, String)> = vec![];
        for (id, state) in &self.active {
            for (key, u) in &state.units {
                if u.status() != UnitStatus::Dispatched {
                    continue;
                }
                let session = u.worker_session.clone().unwrap_or_default();
                let alive = if let Some(inst) = session.strip_prefix(BUILTIN_SESSION_PREFIX) {
                    inst == self.instance
                } else {
                    self.sessions.get(&session).is_some_and(|s| now < s.last_seen_ms + timeout)
                };
                let since = u.dispatched_at_ms.unwrap_or(0).max(self.started_at_ms);
                if !alive && now >= since + timeout {
                    let reason = if session.starts_with(BUILTIN_SESSION_PREFIX) {
                        "runtime restarted while the built-in task was running".to_string()
                    } else {
                        format!("worker `{}` stopped responding", u.worker_id.clone().unwrap_or_default())
                    };
                    lost.push((id.clone(), key.clone(), u.attempt, reason));
                }
            }
        }
        for (id, key, attempt, reason) in lost {
            tracing::warn!(execution_id = %id, step = %key, attempt, reason = %reason, "attempt outcome uncertain; redelivering");
            self.commit(&id, vec![Event::StepAttemptUncertain { step: key, attempt, reason }])?;
            self.advance(&id)?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- workers

    pub fn register_worker(&mut self, req: RegisterWorker) -> Result<Registration> {
        if req.worker_id.trim().is_empty() {
            return Err(EngineError::new(400, "INVALID_REQUEST", "worker_id is required"));
        }
        if req.contracts.is_empty() {
            return Err(EngineError::new(400, "INVALID_REQUEST", "a worker must announce at least one task contract"));
        }
        let now = self.now();
        let mut warnings = vec![];
        for c in &req.contracts {
            if c.task_name.starts_with("orbit.") {
                return Err(EngineError::new(400, "RESERVED_TASK", format!("`{}` is a built-in task", c.task_name)));
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
                contracts: req.contracts.iter().map(|c| (c.task_name.clone(), c.contract_version.clone())).collect(),
                last_seen_ms: now,
                inflight: HashSet::new(),
            },
        );
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
        let now = self.now();
        let s = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| EngineError::new(404, "UNKNOWN_SESSION", "unknown or expired worker session; register again"))?;
        s.last_seen_ms = now;
        let wid = s.worker_id.clone();
        let _ = self.store.touch_worker(&wid, now);
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
                Some(u.status() == UnitStatus::Dispatched && u.token.as_deref() == Some(token.as_str()) && (st.abort.is_none() || key.starts_with("compensate:")))
            });
            if live != Some(true) {
                cancel.push(token.clone());
                self.sessions.get_mut(session_id).unwrap().inflight.remove(&token);
            }
        }
        Ok(HeartbeatAck { accepted: true, cancel })
    }

    pub fn unregister_worker(&mut self, session_id: &str) -> Result<()> {
        let s = self.sessions.remove(session_id).ok_or_else(|| EngineError::new(404, "UNKNOWN_SESSION", "unknown worker session"))?;
        tracing::info!(worker_id = %s.worker_id, "worker unregistered");
        // In-flight work of a departing worker is redelivered immediately.
        let mut lost = vec![];
        for token in s.inflight {
            if let Some((id, key)) = self.tokens.get(&token) {
                if let Some(u) = self.active.get(id).and_then(|st| st.units.get(key)) {
                    if u.status() == UnitStatus::Dispatched && u.token.as_deref() == Some(token.as_str()) {
                        lost.push((id.clone(), key.clone(), u.attempt));
                    }
                }
            }
        }
        for (id, key, attempt) in lost {
            self.commit(&id, vec![Event::StepAttemptUncertain { step: key, attempt, reason: "worker unregistered".into() }])?;
            self.advance(&id)?;
        }
        Ok(())
    }

    pub fn workers(&self) -> Result<Vec<WorkerInfo>> {
        let now = self.now();
        let records = self.store.workers().map_err(EngineError::storage)?;
        Ok(records
            .into_iter()
            .map(|record| {
                let live: Vec<&Session> = self
                    .sessions
                    .values()
                    .filter(|s| s.worker_id == record.worker_id && now < s.last_seen_ms + self.config.session_timeout_ms)
                    .collect();
                WorkerInfo { online: !live.is_empty(), sessions: live.len(), inflight: live.iter().map(|s| s.inflight.len()).sum(), record }
            })
            .collect())
    }

    /// Hand the oldest matching queued task to a worker, if any.
    pub fn poll(&mut self, session_id: &str) -> Result<Option<TaskAssignment>> {
        self.touch(session_id)?;
        let session = self.sessions[session_id].clone();
        let mut best: Option<(u64, String, String)> = None;
        for (id, state) in &self.active {
            let Some(def) = self.by_hash.get(&state.definition_hash) else { continue };
            for (key, u) in &state.units {
                if u.status() != UnitStatus::Scheduled {
                    continue;
                }
                let Some(inv) = invocation(def, key) else { continue };
                if !session.contracts.contains(&(inv.task.clone(), inv.contract_version.clone())) {
                    continue;
                }
                if let Some(lang) = &inv.language {
                    if !lang.eq_ignore_ascii_case(&session.runtime) && lang != "any" {
                        continue;
                    }
                }
                let at = u.scheduled_at_ms.unwrap_or(0);
                if best.as_ref().is_none_or(|(b, _, _)| at < *b) {
                    best = Some((at, id.clone(), key.clone()));
                }
            }
        }
        let Some((_, id, key)) = best else { return Ok(None) };
        let assignment = self.dispatch(&id, &key, session_id, &session.worker_id)?;
        if let Some(s) = self.sessions.get_mut(session_id) {
            s.inflight.insert(assignment.token.clone());
        }
        Ok(Some(assignment))
    }

    fn dispatch(&mut self, id: &str, key: &str, session_id: &str, worker_id: &str) -> Result<TaskAssignment> {
        let now = self.now();
        let state = &self.active[id];
        let def = self.by_hash[&state.definition_hash].clone();
        let inv = invocation(&def, key).expect("unit has an invocation").clone();
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

    fn dispatch_builtins(&mut self) -> Result<()> {
        let mut todo = vec![];
        for (id, state) in &self.active {
            let Some(def) = self.by_hash.get(&state.definition_hash) else { continue };
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
        let (id, key, unit, def) = match self.resolve_token(token)? {
            Ok(x) => x,
            Err(ack) => return Ok(ack),
        };
        let attempt = unit.tokens.iter().find(|(t, _)| t == token).map(|(_, a)| *a).unwrap_or(unit.attempt);
        if output.is_null() {
            output = json!({});
        }
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
            Ok(()) => Event::StepSucceeded { step: key.clone(), attempt, output },
            Err(e) => Event::StepAttemptFailed {
                step: key.clone(),
                attempt: unit.attempt,
                error: TaskError::new(
                    codes::OUTPUT_CONTRACT_VIOLATION,
                    format!("worker output for `{}` does not match its contract at {e}", inv.map(|i| i.task).unwrap_or_default()),
                    false,
                ),
            },
        };
        let accepted = matches!(event, Event::StepSucceeded { .. });
        self.commit(&id, vec![event])?;
        self.advance(&id)?;
        Ok(ResultAck { accepted, reason: (!accepted).then(|| "output violates the task contract".into()) })
    }

    /// Record a failed attempt.
    pub fn fail(&mut self, token: &str, error: TaskError) -> Result<ResultAck> {
        let (id, key, unit, _) = match self.resolve_token(token)? {
            Ok(x) => x,
            Err(ack) => return Ok(ack),
        };
        // Failures only count for the attempt currently in flight.
        if unit.token.as_deref() != Some(token) {
            self.commit(&id, vec![Event::StaleResultIgnored { step: key, token: token.into(), outcome: "failure".into() }])?;
            return Ok(ResultAck { accepted: false, reason: Some("stale attempt".into()) });
        }
        self.commit(&id, vec![Event::StepAttemptFailed { step: key, attempt: unit.attempt, error }])?;
        self.advance(&id)?;
        Ok(ResultAck { accepted: true, reason: None })
    }

    #[allow(clippy::type_complexity)]
    fn resolve_token(&mut self, token: &str) -> Result<std::result::Result<(String, String, UnitState, Arc<WorkflowIr>), ResultAck>> {
        let Some((id, key)) = self.tokens.get(token).cloned() else {
            return Ok(Err(ResultAck { accepted: false, reason: Some("unknown token (execution finished or token invalid)".into()) }));
        };
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
            UnitStatus::Dispatched | UnitStatus::Scheduled | UnitStatus::RetryWait | UnitStatus::Uncertain | UnitStatus::AttemptFailed
        );
        if !unresolved {
            self.commit(&id, vec![Event::StaleResultIgnored { step: key, token: token.into(), outcome: "duplicate".into() }])?;
            return Ok(Err(ResultAck { accepted: false, reason: Some("duplicate result; step already resolved".into()) }));
        }
        Ok(Ok((id, key, unit, def)))
    }

    pub fn store_stats(&self) -> Result<Vec<(String, u64)>> {
        self.store.count_by_status().map_err(EngineError::storage)
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}
