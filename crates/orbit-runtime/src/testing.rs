//! In-process workflow simulation, used by `orbit test` and `orbit simulate`.
//!
//! The [`Simulator`] runs the real [`Engine`] over an in-memory store with a
//! manual clock. It acts as the worker for every task (asking a
//! [`TaskHandler`] for each result), starts child workflows like the server
//! does, sends signals as soon as the execution waits for them (or, for
//! signals an `on event` block handles, as soon as it is idle), and advances
//! durable time (sleeps, retry backoff, timeouts) straight to the next
//! wake-up whenever nothing else can happen, so simulations finish instantly
//! and deterministically.

use crate::engine::{ContractAnnouncement, RegisterWorker, TaskAssignment};
use crate::model::*;
use crate::{Engine, EngineConfig, Store};
use orbit_ir::testing::{value_matches, Expectation, MockOutcome, TestCase};
use orbit_ir::{eval, WORKFLOW_CONTRACT_PREFIX};
use orbit_ir::{Action, Scope, SourceLocation, TaskContract, Type, WorkflowIr};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Simulated time starts at 2026-01-01T00:00:00Z.
pub const SIMULATION_EPOCH_MS: u64 = 1_767_225_600_000;
/// Id of the execution a simulation starts (children get derived ids).
pub const ROOT_EXECUTION_ID: &str = "sim";
/// Upper bound on scheduling rounds, so a runaway simulation always ends.
const MAX_ROUNDS: usize = 100_000;
/// Runtime announced by the simulated worker that takes steps without `using`.
const DEFAULT_RUNTIME: &str = "simulator";
/// Simulated time never runs further than this past the epoch (ten years).
const MAX_SIMULATED_MS: u64 = 10 * 365 * 24 * 3600 * 1000;

/// What the simulated worker does with one task.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskOutcome {
    Complete(Value),
    Fail(TaskError),
    /// Never answer; the attempt's timeout (if any) decides what happens.
    Hang,
    /// The simulation cannot continue (e.g. a test did not mock this task).
    Abort(String),
}

/// One task handed to the simulated worker.
pub struct TaskCall<'a> {
    pub assignment: &'a TaskAssignment,
    pub contract: Option<&'a TaskContract>,
    /// 1 for the first time this unit (step, element or compensation) is
    /// dispatched, 2 for the second, and so on.
    pub call: u32,
}

pub trait TaskHandler {
    fn handle(&mut self, call: &TaskCall) -> TaskOutcome;
}

/// One execution of a finished simulation.
#[derive(Debug, Clone, Serialize)]
pub struct SimulatedExecution {
    pub state: ExecutionState,
    pub history: Vec<HistoryEvent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Simulation {
    /// The execution the simulation started.
    pub root: SimulatedExecution,
    /// Child workflow executions, by id.
    pub children: Vec<SimulatedExecution>,
    /// Why the simulation stopped before the execution finished, or other
    /// things that did not go as the caller asked (e.g. unsent signals).
    pub problems: Vec<String>,
    /// Signals that were never sent because nothing waited for them.
    pub unsent_signals: Vec<String>,
    /// Simulated time from start to the end of the simulation.
    pub elapsed_ms: u64,
}

impl Simulation {
    pub fn finished(&self) -> bool {
        self.root.state.status.is_terminal()
    }
}

pub struct Simulator {
    engine: Engine,
    clock: Arc<AtomicU64>,
    /// (runtime, session id) of the simulated workers.
    sessions: Vec<(String, String)>,
    contracts: HashMap<String, TaskContract>,
}

impl Simulator {
    /// Register `workflows` (children before parents) with a fresh engine.
    pub fn new(workflows: &[WorkflowIr]) -> Result<Simulator, String> {
        let clock = Arc::new(AtomicU64::new(SIMULATION_EPOCH_MS));
        let c = clock.clone();
        let config = EngineConfig {
            // The simulated workers never go missing, however far time jumps.
            session_timeout_ms: 1 << 50,
            heartbeat_interval_ms: 1 << 40,
            // Built-in tasks are handed to the simulator instead of run.
            http_adapter: true,
            ..EngineConfig::default()
        };
        let store = Store::in_memory().map_err(|e| e.to_string())?;
        let mut engine = Engine::open(store, config, Box::new(move || c.load(Ordering::SeqCst)))
            .map_err(|e| e.to_string())?;
        // Register children first: a parent pins its children's definitions.
        let mut pending: Vec<&WorkflowIr> = workflows.iter().collect();
        while !pending.is_empty() {
            let mut next = vec![];
            let mut last_error = None;
            for wf in &pending {
                match engine.register_definition((*wf).clone()) {
                    Ok(_) => {}
                    Err(e) if e.code == "CHILD_NOT_REGISTERED" => {
                        last_error = Some(e.message.clone());
                        next.push(*wf);
                    }
                    Err(e) => return Err(format!("{}@{}: {}", wf.name, wf.version, e.message)),
                }
            }
            if next.len() == pending.len() {
                return Err(last_error.unwrap_or_default());
            }
            pending = next;
        }
        // One simulated worker per language that steps ask for (`using`),
        // plus one for steps that do not care, all serving every task.
        let mut contracts = HashMap::new();
        let mut runtimes = BTreeSet::from([DEFAULT_RUNTIME.to_string()]);
        for wf in workflows {
            for c in wf.contracts.values() {
                if !c.name.starts_with(WORKFLOW_CONTRACT_PREFIX) {
                    contracts.insert(c.name.clone(), c.clone());
                }
            }
            for s in &wf.steps {
                for inv in s.invocation().into_iter().chain(s.compensation.as_ref()) {
                    if let Some(l) = &inv.language {
                        runtimes.insert(l.to_lowercase());
                    }
                }
            }
        }
        let announced: BTreeSet<(String, String)> = contracts
            .values()
            .filter(|c| !c.builtin)
            .map(|c| (c.name.clone(), c.version.clone()))
            .collect();
        let mut sessions = vec![];
        if !announced.is_empty() {
            for runtime in runtimes {
                let reg = engine
                    .register_worker(RegisterWorker {
                        worker_id: if runtime == DEFAULT_RUNTIME {
                            "simulator".to_string()
                        } else {
                            format!("simulator-{runtime}")
                        },
                        runtime: runtime.clone(),
                        sdk: Some("orbit-simulator".into()),
                        contracts: announced
                            .iter()
                            .map(|(n, v)| ContractAnnouncement {
                                task_name: n.clone(),
                                contract_version: v.clone(),
                            })
                            .collect(),
                    })
                    .map_err(|e| e.message)?;
                sessions.push((runtime, reg.worker_session_id));
            }
        }
        Ok(Simulator {
            engine,
            clock,
            sessions,
            contracts,
        })
    }

    fn now(&self) -> u64 {
        self.clock.load(Ordering::SeqCst)
    }

    /// Run one execution to completion (or until it cannot make progress).
    pub fn run(
        mut self,
        workflow: &str,
        version: Option<&str>,
        input: Value,
        signals: &[(String, Value)],
        handler: &mut dyn TaskHandler,
    ) -> Result<Simulation, String> {
        let root = ROOT_EXECUTION_ID.to_string();
        // Across every run of the execution (continue-as-new).
        let started_at = self.now();
        self.engine
            .start(workflow, version, input, Some(root.clone()))
            .map_err(|e| e.message)?;
        let mut calls: HashMap<(String, String), u32> = HashMap::new();
        let mut problems = vec![];
        let mut next_signal = 0;
        let mut rounds = 0;
        let mut seen_progress = 0;
        'run: loop {
            rounds += 1;
            if rounds > MAX_ROUNDS {
                problems.push(format!(
                    "the simulation did not finish within {MAX_ROUNDS} rounds"
                ));
                break;
            }
            let mut work = self.engine.tick().map_err(|e| e.message)?;
            for (_, session) in self.sessions.clone() {
                while let Some(a) = self.engine.poll(&session).map_err(|e| e.message)? {
                    work.push(a);
                }
            }
            for a in work {
                let n = calls
                    .entry((a.execution_id.clone(), a.step_id.clone()))
                    .or_default();
                *n += 1;
                let call = TaskCall {
                    assignment: &a,
                    contract: self.contracts.get(&a.task_name),
                    call: *n,
                };
                match handler.handle(&call) {
                    TaskOutcome::Complete(v) => {
                        self.engine.complete(&a.token, v).map_err(|e| e.message)?;
                    }
                    TaskOutcome::Fail(e) => {
                        self.engine.fail(&a.token, e).map_err(|e| e.message)?;
                    }
                    TaskOutcome::Hang => {}
                    TaskOutcome::Abort(msg) => {
                        problems.push(msg);
                        break 'run;
                    }
                }
            }
            let state = self.engine.execution(&root).map_err(|e| e.message)?;
            if state.status.is_terminal() {
                break;
            }
            // Anything recorded this round (results, children started or
            // finished, timers fired) may enable more work right away.
            let progress = self.progress()?;
            if progress != seen_progress {
                seen_progress = progress;
                continue;
            }
            // Send the next signal once the execution waits for it.
            if let Some((name, payload)) = signals.get(next_signal) {
                if self.awaiting(&state).contains(name.as_str()) {
                    self.engine
                        .signal(&root, name, payload.clone())
                        .map_err(|e| e.message)?;
                    next_signal += 1;
                    continue;
                }
            }
            // Nothing can happen now: jump to the next durable wake-up.
            match self.next_wake_up()? {
                Some(at) if at - SIMULATION_EPOCH_MS <= MAX_SIMULATED_MS => {
                    self.clock.store(at, Ordering::SeqCst);
                }
                Some(_) => {
                    problems.push("the simulation ran past ten years of simulated time".into());
                    break;
                }
                None => {
                    problems.push(self.stuck_reason(&state, signals.get(next_signal)));
                    break;
                }
            }
        }
        let unsent_signals: Vec<String> = signals[next_signal.min(signals.len())..]
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        let mut children = vec![];
        for s in self
            .engine
            .list_executions(None, None, 1000)
            .map_err(|e| e.message)?
        {
            if s.execution_id != root {
                children.push(self.collect(&s.execution_id)?);
            }
        }
        children.sort_by(|a, b| a.state.id.cmp(&b.state.id));
        let root = self.collect(&root)?;
        let end = root.state.finished_at_ms.unwrap_or(self.now());
        Ok(Simulation {
            elapsed_ms: end.saturating_sub(started_at),
            root,
            children,
            problems,
            unsent_signals,
        })
    }

    /// Total number of events recorded across all executions.
    fn progress(&self) -> Result<u64, String> {
        let mut total = 0;
        for s in self
            .engine
            .list_executions(None, None, 1000)
            .map_err(|e| e.message)?
        {
            // Each run's history starts again at 1 (continue-as-new).
            let state = self
                .engine
                .execution(&s.execution_id)
                .map_err(|e| e.message)?;
            total += 1 + state.last_seq + (state.run as u64) * (1 << 32);
        }
        Ok(total)
    }

    fn collect(&self, id: &str) -> Result<SimulatedExecution, String> {
        Ok(SimulatedExecution {
            state: self.engine.execution(id).map_err(|e| e.message)?,
            history: self.engine.history(id).map_err(|e| e.message)?,
        })
    }

    fn definition(&self, state: &ExecutionState) -> Option<Arc<WorkflowIr>> {
        self.engine
            .definition(&state.workflow, Some(&state.version))
    }

    /// Signals the execution's waiting `await` steps want.
    fn awaiting(&self, state: &ExecutionState) -> BTreeSet<String> {
        let Some(def) = self.definition(state) else {
            return BTreeSet::new();
        };
        // Signals handled by `on event` are accepted whenever the execution is
        // running and not shutting down.
        let handled = def.events.keys().filter(|_| state.abort.is_none()).cloned();
        state
            .units
            .iter()
            .filter(|(_, u)| u.status() == UnitStatus::Waiting)
            .filter_map(|(key, _)| awaited(&def, key).map(str::to_string))
            .chain(handled)
            .collect()
    }

    /// The earliest future time at which a timer, retry, deadline or timeout
    /// of any unfinished execution fires.
    fn next_wake_up(&self) -> Result<Option<u64>, String> {
        let now = self.now();
        let mut best: Option<u64> = None;
        let mut consider = |t: u64| {
            if t > now && best.is_none_or(|b| t < b) {
                best = Some(t);
            }
        };
        for s in self
            .engine
            .list_executions(None, None, 1000)
            .map_err(|e| e.message)?
        {
            let state = self
                .engine
                .execution(&s.execution_id)
                .map_err(|e| e.message)?;
            if state.status.is_terminal() {
                continue;
            }
            let def = self.definition(&state);
            if let (Some(def), None) = (&def, &state.abort) {
                if let Some(t) = def.timeout_ms {
                    consider(state.created_at_ms + t);
                }
            }
            for (key, u) in &state.units {
                match u.status() {
                    UnitStatus::Waiting | UnitStatus::RetryWait => {
                        if let Some(t) = u.wake_at_ms {
                            consider(t);
                        }
                    }
                    UnitStatus::Dispatched => {
                        if let Some(t) = u.deadline_ms {
                            consider(t);
                        }
                    }
                    UnitStatus::Scheduled => {
                        let qt = def
                            .as_ref()
                            .and_then(|d| crate::decide::invocation(d, key))
                            .and_then(|i| i.queue_timeout_ms);
                        if let (Some(qt), Some(at), false) = (qt, u.scheduled_at_ms, u.redelivery) {
                            consider(at + qt);
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(best)
    }

    fn stuck_reason(&self, state: &ExecutionState, next: Option<&(String, Value)>) -> String {
        let def = self.definition(state);
        let mut waits = vec![];
        for (key, u) in &state.units {
            match u.status() {
                UnitStatus::Waiting => {
                    if let Some(sig) = def.as_ref().and_then(|d| awaited(d, key)) {
                        let detail = match next {
                            Some((n, _)) if n != sig => format!(
                                "signal `{sig}`, but the next signal to send is `{n}` (signals are sent in order)"
                            ),
                            Some(_) => format!("signal `{sig}`"),
                            None => format!("signal `{sig}`, and no signal is left to send"),
                        };
                        waits.push(format!("step `{key}` waits for {detail}"));
                    }
                }
                UnitStatus::Dispatched => {
                    let task = def
                        .as_ref()
                        .and_then(|d| crate::decide::invocation(d, key))
                        .map(|i| i.task.clone())
                        .unwrap_or_default();
                    if !task.starts_with(WORKFLOW_CONTRACT_PREFIX) {
                        waits.push(format!(
                            "step `{key}` waits for task `{task}`, which never answers and has no timeout"
                        ));
                    }
                }
                _ => {}
            }
        }
        if waits.is_empty() {
            format!(
                "the execution stopped making progress while {}",
                state.status.as_str()
            )
        } else {
            format!("the execution cannot make progress: {}", waits.join("; "))
        }
    }
}

/// The signal an `await` unit waits for.
fn awaited<'a>(def: &'a WorkflowIr, key: &str) -> Option<&'a str> {
    let k = parse_key(key);
    if k.compensation {
        return None;
    }
    match &def.step(k.step)?.action {
        Action::Await { signal, .. } => Some(signal),
        _ => None,
    }
}

/// Run `workflow` once with `handler` acting as every worker.
pub fn simulate(
    workflows: &[WorkflowIr],
    workflow: &str,
    version: Option<&str>,
    input: Value,
    signals: &[(String, Value)],
    handler: &mut dyn TaskHandler,
) -> Result<Simulation, String> {
    Simulator::new(workflows)?.run(workflow, version, input, signals, handler)
}

// ------------------------------------------------------------------- samples

/// A deterministic, type-valid sample value (used by `orbit simulate` for
/// tasks without a mock). `name` is the field the value is for.
///
/// | Type        | Sample                                   |
/// |-------------|------------------------------------------|
/// | `String`    | `"<name>-sample"`                        |
/// | `Int`       | `1`                                      |
/// | `Float`     | `1.0`                                    |
/// | `Bool`      | `true`                                   |
/// | `Bytes`     | `"c2FtcGxl"` (base64 of `sample`)        |
/// | `Duration`  | `1000` (one second)                      |
/// | `Timestamp` | `"2026-01-01T00:00:00Z"`                 |
/// | `UUID`      | `"00000000-0000-4000-8000-000000000001"` |
/// | `JSON`      | `{}`                                     |
/// | `List<T>`   | one sample element                       |
/// | `Map<String, T>` | `{ "key": <sample T> }`             |
/// | `T?`        | `null`                                   |
/// | record      | every field: its default, else a sample  |
pub fn sample_value(ty: &Type, name: &str) -> Value {
    match ty {
        Type::String => json!(format!("{name}-sample")),
        Type::Int => json!(1),
        Type::Float => json!(1.0),
        Type::Bool => json!(true),
        Type::Bytes => json!("c2FtcGxl"),
        Type::Duration => json!(1000),
        Type::Timestamp => json!("2026-01-01T00:00:00Z"),
        Type::Uuid => json!("00000000-0000-4000-8000-000000000001"),
        Type::Json => json!({}),
        Type::Null | Type::Optional { .. } => Value::Null,
        Type::List { item } => json!([sample_value(item, name)]),
        Type::Map { value, .. } => json!({ "key": sample_value(value, name) }),
        Type::Record { fields, .. } => Value::Object(
            fields
                .iter()
                .map(|f| {
                    let v = f
                        .default
                        .clone()
                        .unwrap_or_else(|| sample_value(&f.ty, &f.name));
                    (f.name.clone(), v)
                })
                .collect(),
        ),
    }
}

/// The worker used by `orbit simulate`: fixed results and failures by task
/// name, and a generated sample for every other task.
#[derive(Debug, Clone, Default)]
pub struct SampleHandler {
    /// Task name -> output to return.
    pub results: BTreeMap<String, Value>,
    /// Task name -> error code to fail with (permanently).
    pub failures: BTreeMap<String, String>,
}

impl TaskHandler for SampleHandler {
    fn handle(&mut self, call: &TaskCall) -> TaskOutcome {
        let task = &call.assignment.task_name;
        if let Some(code) = self.failures.get(task) {
            return TaskOutcome::Fail(TaskError::new(
                code,
                format!("simulated failure of `{task}` (--fail)"),
                false,
            ));
        }
        if let Some(v) = self.results.get(task) {
            return TaskOutcome::Complete(v.clone());
        }
        TaskOutcome::Complete(
            call.contract
                .map(|c| sample_value(&c.output, "value"))
                .unwrap_or_else(|| json!({})),
        )
    }
}

// --------------------------------------------------------------------- tests

/// The worker used by `orbit test`: answers from the test's mocks.
pub struct MockHandler<'a> {
    pub test: &'a TestCase,
}

struct TaskScope<'a> {
    input: &'a Value,
    assignment: &'a TaskAssignment,
}

impl Scope for TaskScope<'_> {
    fn input(&self) -> &Value {
        self.input
    }
    fn step_output(&self, _: &str) -> Option<&Value> {
        None
    }
    fn execution_field(&self, field: &str) -> Option<Value> {
        Some(Value::String(match field {
            "id" => self.assignment.execution_id.clone(),
            "workflow" => self.assignment.workflow.clone(),
            "version" => self.assignment.workflow_version.clone(),
            _ => return None,
        }))
    }
}

impl TaskHandler for MockHandler<'_> {
    fn handle(&mut self, call: &TaskCall) -> TaskOutcome {
        let a = call.assignment;
        let Some(behaviours) = self.test.mocks.get(&a.task_name) else {
            return TaskOutcome::Abort(format!(
                "task `{}` (step `{}`) is not mocked; add `mock \"{}\" returns {{ ... }}` to the test",
                a.task_name, a.step_id, a.task_name
            ));
        };
        let mut covered = 0u32;
        let mut chosen = behaviours.last();
        for b in behaviours {
            match b.times {
                Some(n) => {
                    covered = covered.saturating_add(n);
                    if call.call <= covered {
                        chosen = Some(b);
                        break;
                    }
                }
                None => {
                    chosen = Some(b);
                    break;
                }
            }
        }
        match chosen.map(|b| &b.outcome) {
            Some(MockOutcome::Returns { output }) => {
                let scope = TaskScope {
                    input: &a.input,
                    assignment: a,
                };
                match eval(output, &scope) {
                    Ok(v) => TaskOutcome::Complete(v),
                    Err(e) => TaskOutcome::Abort(format!(
                        "the mock of `{}` (step `{}`) failed to evaluate: {e}",
                        a.task_name, a.step_id
                    )),
                }
            }
            Some(MockOutcome::Fails {
                code,
                message,
                retryable,
            }) => TaskOutcome::Fail(TaskError::new(code, message.clone(), *retryable)),
            Some(MockOutcome::Hangs) => TaskOutcome::Hang,
            None => TaskOutcome::Abort(format!("the mock of `{}` has no behaviour", a.task_name)),
        }
    }
}

/// The outcome of one test.
#[derive(Debug, Clone, Serialize)]
pub struct TestResult {
    pub name: String,
    pub workflow: String,
    pub version: String,
    pub location: SourceLocation,
    pub passed: bool,
    /// One message per failed expectation (or why the test could not run).
    pub failures: Vec<String>,
    #[serde(skip)]
    pub simulation: Option<Simulation>,
}

/// Run one compiled test against `workflows` (which must include the tested
/// workflow and its children).
pub fn run_test(workflows: &[WorkflowIr], test: &TestCase) -> TestResult {
    let mut result = TestResult {
        name: test.name.clone(),
        workflow: test.workflow.clone(),
        version: test.version.clone(),
        location: test.location.clone(),
        passed: false,
        failures: vec![],
        simulation: None,
    };
    let signals: Vec<(String, Value)> = test
        .signals
        .iter()
        .map(|s| (s.name.clone(), s.payload.clone()))
        .collect();
    let mut handler = MockHandler { test };
    let sim = match simulate(
        workflows,
        &test.workflow,
        Some(&test.version),
        test.input.clone(),
        &signals,
        &mut handler,
    ) {
        Ok(s) => s,
        Err(e) => {
            result
                .failures
                .push(format!("could not start the execution: {e}"));
            return result;
        }
    };
    result.failures.extend(sim.problems.iter().cloned());
    if sim.finished() {
        for e in &test.expectations {
            if let Some(f) = check(e, &sim.root.state) {
                result.failures.push(f);
            }
        }
        for s in &sim.unsent_signals {
            result.failures.push(format!(
                "signal `{s}` was never sent: the execution finished without waiting for it"
            ));
        }
    }
    result.passed = result.failures.is_empty();
    result.simulation = Some(sim);
    result
}

fn compact(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

fn status_name(s: UnitStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn describe_end(state: &ExecutionState) -> String {
    match &state.error {
        Some(e) => format!("{} ({}: {})", state.status.as_str(), e.code, e.message),
        None => state.status.as_str().to_string(),
    }
}

/// `None` if the expectation holds, else what went wrong. A step written
/// `on_event:<signal>:<step>` stands for that step in every instance of the
/// block: the expectation must hold for each, and at least one must exist.
fn check(e: &Expectation, state: &ExecutionState) -> Option<String> {
    let event_step = match e {
        Expectation::StepStatus { step, .. }
        | Expectation::StepOutput { step, .. }
        | Expectation::StepAttempts { step, .. } => step
            .strip_prefix("on_event:")
            .and_then(|r| r.split_once(':'))
            .map(|(signal, id)| (step.clone(), signal.to_string(), id.to_string())),
        _ => None,
    };
    if let Some((written, signal, id)) = event_step {
        let instances: Vec<String> = state
            .signals
            .iter()
            .filter_map(|r| r.delivered_to.clone())
            .filter(|k| k.starts_with(&format!("on_event:{signal}@")))
            .collect();
        if instances.is_empty() {
            return Some(format!(
                "expected step `{written}`, but no `{signal}` signal started the `on event {signal}` block"
            ));
        }
        for instance in instances {
            let key = format!("{instance}:{id}");
            let mut one = e.clone();
            match &mut one {
                Expectation::StepStatus { step, .. }
                | Expectation::StepOutput { step, .. }
                | Expectation::StepAttempts { step, .. } => *step = key,
                _ => {}
            }
            if let Some(why) = check_one(&one, state) {
                return Some(why.replace(&format!("{instance}:{id}"), &written));
            }
        }
        return None;
    }
    check_one(e, state)
}

fn check_one(e: &Expectation, state: &ExecutionState) -> Option<String> {
    let unit = |step: &str| state.units.get(step).cloned().unwrap_or_default();
    match e {
        Expectation::Runs { runs } => (state.run != *runs).then(|| {
            format!(
                "expected the execution to end in run {runs}, it ended in run {}",
                state.run
            )
        }),
        Expectation::Status { status } => (state.status.as_str() != status)
            .then(|| format!("expected status {status}, got {}", describe_end(state))),
        Expectation::Output { expected } => {
            let actual = state.output.clone().unwrap_or(Value::Null);
            (!value_matches(expected, &actual)).then(|| {
                if state.output.is_none() {
                    format!(
                        "expected output {}, but the workflow produced none (it ended {})",
                        compact(expected),
                        describe_end(state)
                    )
                } else {
                    format!(
                        "expected output {}, got {}",
                        compact(expected),
                        compact(&actual)
                    )
                }
            })
        }
        Expectation::Error { code } => match &state.error {
            Some(err) if err.code == *code => None,
            Some(err) => Some(format!(
                "expected error {code:?}, got {:?} ({})",
                err.code, err.message
            )),
            None => Some(format!(
                "expected error {code:?}, but the workflow ended {} without an error",
                state.status.as_str()
            )),
        },
        Expectation::StepStatus { step, status } => {
            let actual = status_name(unit(step).status());
            (actual != *status).then(|| {
                let u = unit(step);
                let why = u
                    .error
                    .as_ref()
                    .map(|e| format!(" ({}: {})", e.code, e.message))
                    .unwrap_or_default();
                format!("expected step `{step}` to be {status}, got {actual}{why}")
            })
        }
        Expectation::StepOutput { step, expected } => {
            let u = unit(step);
            let actual = u.output.clone().unwrap_or(Value::Null);
            (!value_matches(expected, &actual)).then(|| {
                format!(
                    "expected step `{step}` output {}, got {} (step is {})",
                    compact(expected),
                    compact(&actual),
                    status_name(u.status())
                )
            })
        }
        Expectation::StepAttempts { step, attempts } => {
            let actual = unit(step).attempt;
            (actual != *attempts).then(|| {
                format!("expected step `{step}` to take {attempts} attempt(s), took {actual}")
            })
        }
        Expectation::Compensated { step } => {
            let element = format!("{}[", compensation_key(step));
            let units: Vec<(&String, &UnitState)> = state
                .units
                .iter()
                .filter(|(k, _)| **k == compensation_key(step) || k.starts_with(&element))
                .collect();
            if units.is_empty() {
                return Some(format!(
                    "expected step `{step}` to be compensated, but its compensation never ran"
                ));
            }
            units
                .iter()
                .find(|(_, u)| u.status() != UnitStatus::Succeeded)
                .map(|(k, u)| {
                    let why = u
                        .error
                        .as_ref()
                        .map(|e| format!(" ({}: {})", e.code, e.message))
                        .unwrap_or_default();
                    format!(
                        "expected step `{step}` to be compensated, but `{k}` is {}{why}",
                        status_name(u.status())
                    )
                })
        }
    }
}
