//! Runtime semantics tests (RFC-0001 §9, §15.2, §15.4) driven by a manual clock.

use orbit_ir::WorkflowIr;
use orbit_runtime::engine::{ContractAnnouncement, RegisterWorker, TaskAssignment};
use orbit_runtime::model::{
    CompensationOutcome, Event, ExecutionState, TaskError, UnitStatus, WorkflowStatus,
};
use orbit_runtime::{Engine, EngineConfig, Store};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const CONTRACTS: &str = r#"
task "a" { version "1.0.0" input { n: Int } output { n: Int } }
task "b" { version "1.0.0" input { n: Int } output { n: Int } }
task "undo" { version "1.0.0" input { n: Int } output { ok: Bool } }
"#;

fn compile(src: &str) -> WorkflowIr {
    let c = orbit_compiler::compile_str("t.orbit", &format!("{CONTRACTS}\n{src}"));
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    c.workflows.into_iter().next().unwrap()
}

struct Harness {
    engine: Engine,
    clock: Arc<AtomicU64>,
    db: Option<PathBuf>,
}

impl Harness {
    fn new() -> Self {
        Self::with_store(
            Store::in_memory().unwrap(),
            Arc::new(AtomicU64::new(1_000_000)),
            None,
        )
    }

    fn on_disk(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("orbit-test-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("orbit.db");
        let _ = std::fs::remove_file(&db);
        Self::with_store(
            Store::open(&db).unwrap(),
            Arc::new(AtomicU64::new(1_000_000)),
            Some(db),
        )
    }

    fn with_store(store: Store, clock: Arc<AtomicU64>, db: Option<PathBuf>) -> Self {
        let c = clock.clone();
        let engine = Engine::open(
            store,
            EngineConfig::default(),
            Box::new(move || c.load(Ordering::SeqCst)),
        )
        .unwrap();
        Harness { engine, clock, db }
    }

    /// Simulate a crash: drop the engine (no shutdown logic) and reopen the database.
    fn restart(self) -> Self {
        let db = self.db.clone().expect("on-disk harness");
        let clock = self.clock.clone();
        drop(self.engine);
        Self::with_store(Store::open(&db).unwrap(), clock, Some(db))
    }

    fn advance(&mut self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
        self.engine.tick().unwrap();
    }

    fn worker(&mut self, id: &str, runtime: &str, tasks: &[&str]) -> String {
        self.engine
            .register_worker(RegisterWorker {
                worker_id: id.into(),
                runtime: runtime.into(),
                sdk: None,
                contracts: tasks
                    .iter()
                    .map(|t| ContractAnnouncement {
                        task_name: t.to_string(),
                        contract_version: "1.0.0".into(),
                    })
                    .collect(),
            })
            .unwrap()
            .worker_session_id
    }

    fn poll(&mut self, session: &str) -> Option<TaskAssignment> {
        self.engine.poll(session).unwrap()
    }

    fn start(&mut self, ir: &WorkflowIr, input: Value) -> String {
        self.engine.register_definition(ir.clone()).unwrap();
        self.engine
            .start(&ir.name, None, input, None)
            .unwrap()
            .execution_id
    }

    fn state(&self, id: &str) -> ExecutionState {
        self.engine.execution(id).unwrap()
    }

    fn events(&self, id: &str) -> Vec<&'static str> {
        self.engine
            .history(id)
            .unwrap()
            .iter()
            .map(|h| h.event.name())
            .collect()
    }
}

fn echo(h: &mut Harness, session: &str) -> TaskAssignment {
    let t = h.poll(session).expect("a task");
    let n = t.input["n"].as_i64().unwrap();
    assert!(
        h.engine
            .complete(&t.token, json!({ "n": n + 1 }))
            .unwrap()
            .accepted
    );
    t
}

#[test]
fn independent_steps_run_concurrently_and_results_flow() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0" input { n: Int }
        step left { worker "a" with { n: input.n } }
        step right { worker "b" with { n: input.n * 10 } }
        step join { worker "a" with { n: left.n + right.n } }
        return { total: join.n } }"#,
    );
    let id = h.start(&ir, json!({ "n": 1 }));
    // Both independent steps are queued at once.
    let s = h.state(&id);
    assert_eq!(s.units["left"].status(), UnitStatus::Scheduled);
    assert_eq!(s.units["right"].status(), UnitStatus::Scheduled);
    assert!(!s.units.contains_key("join"));

    let wa = h.worker("wa", "python", &["a"]);
    let wb = h.worker("wb", "typescript", &["b"]);
    let ta = h.poll(&wa).unwrap();
    let tb = h.poll(&wb).unwrap();
    assert_eq!(
        (ta.step_id.as_str(), tb.step_id.as_str()),
        ("left", "right")
    );
    assert_eq!(tb.input, json!({ "n": 10 }));
    // Default idempotency key is stable per execution and step.
    assert_eq!(ta.idempotency_key, format!("{id}/left"));
    h.engine.complete(&tb.token, json!({ "n": 11 })).unwrap();
    assert!(h.poll(&wa).is_none(), "join must wait for left");
    h.engine.complete(&ta.token, json!({ "n": 2 })).unwrap();
    let tj = h.poll(&wa).unwrap();
    assert_eq!(tj.input, json!({ "n": 13 }));
    h.engine.complete(&tj.token, json!({ "n": 14 })).unwrap();
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Completed);
    assert_eq!(s.output, Some(json!({ "total": 14 })));
}

#[test]
fn retries_with_exponential_backoff() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0"
        step s { worker "a" with { n: 1 } retry { attempts: 3, backoff: exponential, initial_delay: 1s, max_delay: 10s } } }"#,
    );
    let id = h.start(&ir, json!({}));
    let w = h.worker("w", "python", &["a"]);
    let t1 = h.poll(&w).unwrap();
    assert_eq!(t1.attempt, 1);
    h.engine
        .fail(&t1.token, TaskError::new("FLAKY", "boom", true))
        .unwrap();
    assert_eq!(h.state(&id).units["s"].status(), UnitStatus::RetryWait);
    assert!(h.poll(&w).is_none(), "retry waits for its delay");
    h.advance(999);
    assert!(h.poll(&w).is_none());
    h.advance(1);
    let t2 = h.poll(&w).unwrap();
    assert_eq!(t2.attempt, 2);
    assert_eq!(
        t2.idempotency_key, t1.idempotency_key,
        "retries reuse the idempotency key"
    );
    h.engine
        .fail(&t2.token, TaskError::new("FLAKY", "boom", true))
        .unwrap();
    h.advance(1_999);
    assert!(h.poll(&w).is_none());
    h.advance(1);
    let t3 = h.poll(&w).unwrap();
    assert_eq!(t3.attempt, 3);
    h.engine
        .fail(&t3.token, TaskError::new("FLAKY", "boom", true))
        .unwrap();
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Failed);
    assert!(s
        .error
        .unwrap()
        .message
        .contains("gave up after 3 attempts"));
}

#[test]
fn non_retryable_errors_fail_immediately() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0"
        step s { worker "a" with { n: 1 } retry { attempts: 5, non_retryable: ["DECLINED"] } } }"#,
    );
    let id = h.start(&ir, json!({}));
    let w = h.worker("w", "python", &["a"]);
    let t = h.poll(&w).unwrap();
    h.engine
        .fail(&t.token, TaskError::new("DECLINED", "card declined", true))
        .unwrap();
    assert_eq!(h.state(&id).status, WorkflowStatus::Failed);
}

#[test]
fn attempt_timeouts_trigger_retry() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0" step s { worker "a" with { n: 1 } timeout 5s retry { attempts: 2, initial_delay: 1s } } }"#,
    );
    let id = h.start(&ir, json!({}));
    let w = h.worker("w", "python", &["a"]);
    let t1 = h.poll(&w).unwrap();
    assert!(t1.deadline_ms.is_some());
    h.advance(5_000);
    let s = h.state(&id);
    assert_eq!(s.units["s"].status(), UnitStatus::RetryWait);
    assert_eq!(s.units["s"].last_error.as_ref().unwrap().code, "TIMEOUT");
    // The worker learns that the timed-out attempt should stop.
    assert_eq!(
        h.engine.heartbeat(&w).unwrap().cancel,
        vec![t1.token.clone()]
    );
    h.advance(1_000);
    let t2 = h.poll(&w).unwrap();
    h.engine.complete(&t2.token, json!({ "n": 1 })).unwrap();
    assert_eq!(h.state(&id).status, WorkflowStatus::Completed);
}

#[test]
fn failure_compensates_completed_steps_in_reverse_order() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0"
        step one { worker "a" with { n: 1 } compensate { worker "undo" with { n: one.n } } }
        step two { worker "a" after one with { n: 2 } compensate { worker "undo" with { n: two.n } } }
        step three { worker "b" after two with { n: 3 } } }"#,
    );
    let id = h.start(&ir, json!({}));
    let w = h.worker("w", "go", &["a", "b", "undo"]);
    echo(&mut h, &w);
    echo(&mut h, &w);
    let t = h.poll(&w).unwrap();
    assert_eq!(t.step_id, "three");
    h.engine
        .fail(&t.token, TaskError::new("OUT_OF_STOCK", "nope", false))
        .unwrap();
    assert_eq!(h.state(&id).status, WorkflowStatus::Compensating);
    let c1 = h.poll(&w).unwrap();
    assert_eq!(c1.step_id, "compensate:two");
    assert_eq!(c1.input, json!({ "n": 3 }));
    assert!(h.poll(&w).is_none(), "compensations run one at a time");
    h.engine.complete(&c1.token, json!({ "ok": true })).unwrap();
    let c2 = h.poll(&w).unwrap();
    assert_eq!(c2.step_id, "compensate:one");
    h.engine.complete(&c2.token, json!({ "ok": true })).unwrap();
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Failed);
    assert_eq!(s.compensation, Some(CompensationOutcome::Completed));
    assert_eq!(s.error.unwrap().code, "OUT_OF_STOCK");
    let ev = h.events(&id);
    assert!(ev.contains(&"CompensationStarted") && ev.contains(&"CompensationCompleted"));
}

#[test]
fn cancellation_withdraws_queued_work_and_compensates() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0"
        step one { worker "a" with { n: 1 } compensate { worker "undo" with { n: one.n } } }
        step two { worker "b" after one with { n: 2 } } }"#,
    );
    let id = h.start(&ir, json!({}));
    let w = h.worker("w", "go", &["a", "b", "undo"]);
    echo(&mut h, &w);
    let s = h.engine.cancel(&id, "customer changed their mind").unwrap();
    assert_eq!(s.units["two"].status(), UnitStatus::Cancelled);
    let c = h.poll(&w).unwrap();
    assert_eq!(c.step_id, "compensate:one");
    h.engine.complete(&c.token, json!({ "ok": true })).unwrap();
    assert_eq!(h.state(&id).status, WorkflowStatus::Cancelled);
}

#[test]
fn conditional_steps_are_skipped_and_read_as_null() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0" input { go: Bool }
        step maybe { worker "a" when input.go with { n: 1 } }
        step after_it { worker "b" after maybe with { n: coalesce(maybe.n, 0) } }
        output { got: Int? }
        return { got: maybe.n } }"#,
    );
    let id = h.start(&ir, json!({ "go": false }));
    let w = h.worker("w", "go", &["a", "b"]);
    let t = h.poll(&w).unwrap();
    assert_eq!(t.step_id, "after_it");
    assert_eq!(t.input, json!({ "n": 0 }));
    h.engine.complete(&t.token, json!({ "n": 0 })).unwrap();
    let s = h.state(&id);
    assert_eq!(s.units["maybe"].status(), UnitStatus::Skipped);
    assert_eq!(s.output, Some(json!({ "got": null })));
}

#[test]
fn crash_recovery_resumes_and_reconciles_in_flight_work() {
    let mut h = Harness::on_disk("recovery");
    let ir = compile(
        r#"workflow W { version "1.0.0"
        step first { worker "a" with { n: 1 } }
        step second { worker "a" after first with { n: first.n } } }"#,
    );
    let id = h.start(&ir, json!({}));
    let w = h.worker("w", "python", &["a"]);
    echo(&mut h, &w);
    let in_flight = h.poll(&w).unwrap();
    let before = serde_json::to_value(h.state(&id)).unwrap();

    // Runtime crashes while `second` is dispatched.
    let mut h = h.restart();
    let after = serde_json::to_value(h.state(&id)).unwrap();
    assert_eq!(before, after, "state rebuilt from history is identical");
    assert_eq!(
        h.state(&id).units["second"].status(),
        UnitStatus::Dispatched
    );

    // The worker never reports back: after the session timeout the outcome is
    // treated as *unknown* and redelivered with the same idempotency key.
    let w2 = h.worker("w", "python", &["a"]);
    h.advance(29_000);
    assert!(h.poll(&w2).is_none());
    h.advance(1_000);
    let redelivered = h.poll(&w2).unwrap();
    assert!(redelivered.redelivery);
    assert_eq!(
        redelivered.attempt, in_flight.attempt,
        "redelivery does not consume a retry attempt"
    );
    assert_eq!(redelivered.idempotency_key, in_flight.idempotency_key);

    // The original worker eventually reports success with its old token: the
    // work really happened, so it is accepted (reconciliation)...
    assert!(
        h.engine
            .complete(&in_flight.token, json!({ "n": 2 }))
            .unwrap()
            .accepted
    );
    assert_eq!(h.state(&id).status, WorkflowStatus::Completed);
    // ...and the duplicate from the redelivery is ignored (finished executions
    // drop their tokens).
    assert!(
        !h.engine
            .complete(&redelivered.token, json!({ "n": 2 }))
            .unwrap()
            .accepted
    );
    assert!(h.events(&id).contains(&"StepAttemptUncertain"));
}

#[test]
fn duplicate_results_are_ignored() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0" step s { worker "a" with { n: 1 } } step t { worker "a" after s with { n: 1 } } }"#,
    );
    let id = h.start(&ir, json!({}));
    let w = h.worker("w", "python", &["a"]);
    let t = h.poll(&w).unwrap();
    assert!(
        h.engine
            .complete(&t.token, json!({ "n": 1 }))
            .unwrap()
            .accepted
    );
    let dup = h.engine.complete(&t.token, json!({ "n": 999 })).unwrap();
    assert!(!dup.accepted);
    assert_eq!(h.state(&id).units["s"].output, Some(json!({ "n": 1 })));
    assert!(h.events(&id).contains(&"StaleResultIgnored"));
}

#[test]
fn worker_output_is_validated_against_the_contract() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0" step s { worker "a" with { n: 1 } retry { attempts: 3 } } }"#,
    );
    let id = h.start(&ir, json!({}));
    let w = h.worker("w", "python", &["a"]);
    let t = h.poll(&w).unwrap();
    let ack = h
        .engine
        .complete(&t.token, json!({ "n": "not a number" }))
        .unwrap();
    assert!(!ack.accepted);
    let s = h.state(&id);
    assert_eq!(
        s.status,
        WorkflowStatus::Failed,
        "contract violations are not retried"
    );
    assert_eq!(s.error.unwrap().code, "OUTPUT_CONTRACT_VIOLATION");
}

#[test]
fn input_is_validated_and_defaults_applied() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0" input { n: Int, label: String = "x" } step s { worker "a" with { n: input.n } } }"#,
    );
    h.engine.register_definition(ir.clone()).unwrap();
    let err = h
        .engine
        .start("W", None, json!({ "n": "1" }), None)
        .unwrap_err();
    assert_eq!(err.code, "INVALID_INPUT");
    let err = h
        .engine
        .start("W", None, json!({ "n": 1, "extra": true }), None)
        .unwrap_err();
    assert!(err.message.contains("unknown field `extra`"));
    let id = h
        .engine
        .start("W", None, json!({ "n": 1 }), None)
        .unwrap()
        .execution_id;
    assert_eq!(h.state(&id).input, json!({ "n": 1, "label": "x" }));
}

#[test]
fn definitions_are_immutable_and_starts_are_idempotent() {
    let mut h = Harness::new();
    let v1 = compile(r#"workflow W { version "1.0.0" step s { worker "a" with { n: 1 } } }"#);
    let v1b = compile(r#"workflow W { version "1.0.0" step s { worker "a" with { n: 2 } } }"#);
    assert!(h.engine.register_definition(v1.clone()).unwrap());
    assert!(
        !h.engine.register_definition(v1.clone()).unwrap(),
        "re-registering the same definition is a no-op"
    );
    assert_eq!(
        h.engine.register_definition(v1b).unwrap_err().code,
        "DEFINITION_CONFLICT"
    );
    let a = h
        .engine
        .start("W", None, json!({}), Some("order-42".into()))
        .unwrap();
    let b = h
        .engine
        .start("W", None, json!({}), Some("order-42".into()))
        .unwrap();
    assert!(a.created && !b.created);
    assert_eq!(h.engine.list_executions(None, None, 10).unwrap().len(), 1);
}

#[test]
fn running_executions_stay_pinned_to_their_version() {
    let mut h = Harness::new();
    let v1 = compile(r#"workflow W { version "1.0.0" step s { worker "a" with { n: 1 } } }"#);
    let v2 = compile(r#"workflow W { version "2.0.0" step s { worker "a" with { n: 2 } } }"#);
    let old = h.start(&v1, json!({}));
    h.advance(1);
    h.engine.register_definition(v2).unwrap();
    let new = h
        .engine
        .start("W", None, json!({}), None)
        .unwrap()
        .execution_id;
    assert_eq!(h.state(&old).version, "1.0.0");
    assert_eq!(h.state(&new).version, "2.0.0");
    let w = h.worker("w", "python", &["a"]);
    let t1 = h.poll(&w).unwrap();
    assert_eq!(t1.input, json!({ "n": 1 }));
}

#[test]
fn durable_timers_survive_restart() {
    let mut h = Harness::on_disk("timers");
    let ir = compile(
        r#"workflow W { version "1.0.0" step wait { sleep 1h } step s { worker "a" after wait with { n: 1 } } }"#,
    );
    let id = h.start(&ir, json!({}));
    h.advance(30 * 60_000);
    let mut h = h.restart();
    let w = h.worker("w", "python", &["a"]);
    assert!(h.poll(&w).is_none());
    h.advance(30 * 60_000);
    let w = h.worker("w", "python", &["a"]);
    assert_eq!(h.state(&id).units["wait"].status(), UnitStatus::Succeeded);
    assert!(h.poll(&w).is_some());
}

#[test]
fn workflow_timeout() {
    let mut h = Harness::new();
    let ir =
        compile(r#"workflow W { version "1.0.0" timeout 1m step s { worker "a" with { n: 1 } } }"#);
    let id = h.start(&ir, json!({}));
    h.advance(60_000);
    assert_eq!(h.state(&id).status, WorkflowStatus::TimedOut);
}

#[test]
fn language_routing() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0" step s { worker "a" using python with { n: 1 } } }"#,
    );
    h.start(&ir, json!({}));
    let ts = h.worker("ts", "typescript", &["a"]);
    let py = h.worker("py", "python", &["a"]);
    assert!(
        h.poll(&ts).is_none(),
        "typescript worker must not receive a python step"
    );
    assert!(h.poll(&py).is_some());
}

#[test]
fn queue_timeout_fails_when_no_worker_arrives() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0" step s { worker "a" with { n: 1 } queue_timeout 10s } }"#,
    );
    let id = h.start(&ir, json!({}));
    h.advance(10_000);
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Failed);
    assert_eq!(s.error.unwrap().code, "QUEUE_TIMEOUT");
}

#[test]
fn http_adapter_disabled_by_default() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0" step s { worker "orbit.http.request" with { url: "http://127.0.0.1:1/" } } }"#,
    );
    let id = h.start(&ir, json!({}));
    h.advance(1);
    assert_eq!(h.state(&id).error.unwrap().code, "ADAPTER_DISABLED");
}

#[test]
fn history_is_authoritative() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0"
        step one { worker "a" with { n: 1 } retry { attempts: 2, initial_delay: 1s } compensate { worker "undo" with { n: 1 } } }
        step two { worker "b" after one with { n: 2 } } }"#,
    );
    let id = h.start(&ir, json!({}));
    let w = h.worker("w", "go", &["a", "b", "undo"]);
    let t = h.poll(&w).unwrap();
    h.engine
        .fail(&t.token, TaskError::new("X", "x", true))
        .unwrap();
    h.advance(1_000);
    echo(&mut h, &w);
    let t = h.poll(&w).unwrap();
    h.engine
        .fail(&t.token, TaskError::new("Y", "y", false))
        .unwrap();
    let c = h.poll(&w).unwrap();
    h.engine.complete(&c.token, json!({ "ok": true })).unwrap();
    let live = serde_json::to_value(h.state(&id)).unwrap();
    let history = h.engine.history(&id).unwrap();
    let replayed = serde_json::to_value(ExecutionState::replay(&id, &history)).unwrap();
    assert_eq!(live, replayed);
    // Sequence numbers are gap-free.
    assert!(history
        .iter()
        .enumerate()
        .all(|(i, e)| e.seq == i as u64 + 1));
    assert!(matches!(
        history.last().unwrap().event,
        Event::WorkflowFailed { .. }
    ));
}

// ------------------------------------------------------------------ fan-out

fn compile_all(src: &str) -> Vec<WorkflowIr> {
    let c = orbit_compiler::compile_str("t.orbit", &format!("{CONTRACTS}\n{src}"));
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    c.workflows
}

#[test]
fn for_each_fans_out_with_max_parallel_and_collects_in_order() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0" input { ns: List<Int> }
        step each { for n in input.ns  max_parallel 2  worker "a" with { n: n * 10 } }
        return { results: each } }"#,
    );
    let id = h.start(&ir, json!({ "ns": [1, 2, 3, 4, 5] }));
    let w = h.worker("w", "go", &["a"]);
    let s = h.state(&id);
    assert_eq!(s.units["each"].status(), UnitStatus::Running);
    let scheduled = |s: &ExecutionState| {
        (0..5)
            .filter(|i| s.units[&format!("each[{i}]")].status() == UnitStatus::Scheduled)
            .count()
    };
    assert_eq!(scheduled(&s), 2, "max_parallel limits concurrency");
    // Complete out of order; results still come back in input order.
    let t0 = h.poll(&w).unwrap();
    let t1 = h.poll(&w).unwrap();
    assert_eq!(
        (t0.step_id.as_str(), t1.step_id.as_str()),
        ("each[0]", "each[1]")
    );
    assert_eq!(t1.idempotency_key, format!("{id}/each[1]"));
    h.engine.complete(&t1.token, json!({ "n": 21 })).unwrap();
    assert!(h.poll(&w).is_some(), "a slot opened up");
    h.engine.complete(&t0.token, json!({ "n": 11 })).unwrap();
    while let Some(t) = h.poll(&w) {
        let n = t.input["n"].as_i64().unwrap();
        h.engine.complete(&t.token, json!({ "n": n + 1 })).unwrap();
    }
    // Drain the task started before the second slot was observed.
    let s = h.state(&id);
    for i in 0..5 {
        let key = format!("each[{i}]");
        if s.units[&key].status() == UnitStatus::Dispatched {
            let t = s.units[&key].token.clone().unwrap();
            let n = s.units[&key].input.as_ref().unwrap()["n"].as_i64().unwrap();
            h.engine.complete(&t, json!({ "n": n + 1 })).unwrap();
        }
    }
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Completed, "{s:#?}");
    assert_eq!(
        s.output,
        Some(
            json!({ "results": [{ "n": 11 }, { "n": 21 }, { "n": 31 }, { "n": 41 }, { "n": 51 }] })
        )
    );
}

#[test]
fn for_each_over_empty_list_succeeds_immediately() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0" input { ns: List<Int> } step each { for n in input.ns worker "a" with { n: n } } return { r: each } }"#,
    );
    let id = h.start(&ir, json!({ "ns": [] }));
    assert_eq!(h.state(&id).output, Some(json!({ "r": [] })));
}

#[test]
fn for_each_failure_compensates_each_succeeded_element() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0" input { ns: List<Int> }
        step each {
            for n in input.ns
            worker "a"
            with { n: n }
            compensate { worker "undo" with { n: each.n + n } }
        } }"#,
    );
    let id = h.start(&ir, json!({ "ns": [1, 2, 3] }));
    let w = h.worker("w", "go", &["a", "undo"]);
    let t: Vec<TaskAssignment> = (0..3).map(|_| h.poll(&w).unwrap()).collect();
    h.engine.complete(&t[0].token, json!({ "n": 100 })).unwrap();
    h.engine
        .fail(
            &t[1].token,
            TaskError::new("NOPE", "element 1 failed", false),
        )
        .unwrap();
    // Element 2 is still in flight: compensation waits for it.
    assert_eq!(h.state(&id).status, WorkflowStatus::Running);
    h.engine.complete(&t[2].token, json!({ "n": 300 })).unwrap();
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Compensating);
    assert_eq!(s.units["each"].status(), UnitStatus::Failed);
    // Newest success first; each compensation sees its own element and output.
    let c1 = h.poll(&w).unwrap();
    assert_eq!(
        (c1.step_id.as_str(), c1.input.clone()),
        ("compensate:each[2]", json!({ "n": 303 }))
    );
    h.engine.complete(&c1.token, json!({ "ok": true })).unwrap();
    let c2 = h.poll(&w).unwrap();
    assert_eq!(
        (c2.step_id.as_str(), c2.input.clone()),
        ("compensate:each[0]", json!({ "n": 101 }))
    );
    h.engine.complete(&c2.token, json!({ "ok": true })).unwrap();
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Failed);
    assert_eq!(s.error.unwrap().code, "NOPE");
    assert_eq!(s.compensation, Some(CompensationOutcome::Completed));
}

// --------------------------------------------------------- child workflows

const PARENT_CHILD: &str = r#"
workflow Child {
    version "1.0.0"
    input { n: Int }
    output { doubled: Int }
    step d { worker "b" with { n: input.n * 2 } }
    return { doubled: d.n }
}
workflow Parent {
    version "1.0.0"
    input { ns: List<Int> }
    output { results: List<{ doubled: Int }>, total: Int }
    step kids { for n in input.ns workflow "Child" with { n: n } }
    step after_kids { worker "a" after kids with { n: len(kids) } }
    return { results: kids, total: after_kids.n }
}
"#;

fn register_all(h: &mut Harness, wfs: &[WorkflowIr]) {
    for wf in wfs {
        h.engine.register_definition(wf.clone()).unwrap();
    }
}

#[test]
fn child_workflows_run_and_return_results() {
    let mut h = Harness::new();
    let wfs = compile_all(PARENT_CHILD);
    assert_eq!(
        wfs.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(),
        vec!["Child", "Parent"],
        "callees first"
    );
    // The parent cannot be registered before its child.
    let err = h.engine.register_definition(wfs[1].clone()).unwrap_err();
    assert_eq!(err.code, "CHILD_NOT_REGISTERED");
    register_all(&mut h, &wfs);
    let id = h
        .engine
        .start("Parent", None, json!({ "ns": [3, 4] }), None)
        .unwrap()
        .execution_id;
    h.advance(1);
    let child_ids = [format!("{id}.kids.0"), format!("{id}.kids.1")];
    for c in &child_ids {
        let s = h.state(c);
        assert_eq!(s.workflow, "Child");
        assert_eq!(s.parent.as_ref().unwrap().execution_id, id);
    }
    let w = h.worker("w", "go", &["a", "b"]);
    for _ in 0..2 {
        let t = h.poll(&w).unwrap();
        let n = t.input["n"].as_i64().unwrap();
        h.engine.complete(&t.token, json!({ "n": n })).unwrap();
    }
    h.advance(1); // reconcile finished children into the parent
    let t = h.poll(&w).unwrap();
    assert_eq!(t.input, json!({ "n": 2 }));
    h.engine.complete(&t.token, json!({ "n": 2 })).unwrap();
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Completed, "{s:#?}");
    assert_eq!(
        s.output,
        Some(json!({ "results": [{ "doubled": 6 }, { "doubled": 8 }], "total": 2 }))
    );
}

#[test]
fn cancelling_a_parent_cancels_its_children() {
    let mut h = Harness::new();
    let wfs = compile_all(PARENT_CHILD);
    register_all(&mut h, &wfs);
    let id = h
        .engine
        .start("Parent", None, json!({ "ns": [1] }), None)
        .unwrap()
        .execution_id;
    h.advance(1);
    let child = format!("{id}.kids.0");
    assert_eq!(h.state(&child).status, WorkflowStatus::Running);
    h.engine.cancel(&id, "stop").unwrap();
    h.advance(1);
    assert_eq!(h.state(&child).status, WorkflowStatus::Cancelled);
    h.advance(1);
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Cancelled, "{s:#?}");
}

#[test]
fn child_failure_fails_the_parent_step() {
    let mut h = Harness::new();
    let wfs = compile_all(PARENT_CHILD);
    register_all(&mut h, &wfs);
    let id = h
        .engine
        .start("Parent", None, json!({ "ns": [1] }), None)
        .unwrap()
        .execution_id;
    h.advance(1);
    let w = h.worker("w", "go", &["a", "b"]);
    let t = h.poll(&w).unwrap();
    h.engine
        .fail(
            &t.token,
            TaskError::new("BROKEN", "child step broke", false),
        )
        .unwrap();
    h.advance(1);
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Failed, "{s:#?}");
    let e = s.error.unwrap();
    assert_eq!(e.code, "BROKEN");
    assert!(
        e.message.contains("child workflow `Child` failed"),
        "{}",
        e.message
    );
}

#[test]
fn child_results_survive_a_parent_crash() {
    let mut h = Harness::on_disk("children");
    let wfs = compile_all(PARENT_CHILD);
    register_all(&mut h, &wfs);
    let id = h
        .engine
        .start("Parent", None, json!({ "ns": [5] }), None)
        .unwrap()
        .execution_id;
    h.advance(1);
    let w = h.worker("w", "go", &["a", "b"]);
    let t = h.poll(&w).unwrap();
    h.engine.complete(&t.token, json!({ "n": 10 })).unwrap();
    // The child finished; crash before the parent hears about it.
    let mut h = h.restart();
    h.advance(1);
    let w = h.worker("w", "go", &["a", "b"]);
    let t = h
        .poll(&w)
        .expect("parent resumed after reconciling its child");
    assert_eq!(t.step_id, "after_kids");
    h.engine.complete(&t.token, json!({ "n": 1 })).unwrap();
    assert_eq!(
        h.state(&id).output,
        Some(json!({ "results": [{ "doubled": 10 }], "total": 1 }))
    );
}

// ---------------------------------------------------------------- signals

const APPROVAL: &str = r#"workflow W { version "1.0.0"
    input { n: Int }
    signal approve { approved: Bool, by: String = "nobody" }
    step wait { await approve timeout 1h }
    step work { worker "a" when wait.approved with { n: input.n } compensate { worker "undo" with { n: work.n } } }
    step after_work { worker "b" after work with { n: 0 } }
    query progress = { approved: wait.approved, by: wait.by, n: work.n }
    return { approved: wait.approved, by: wait.by }
}"#;

#[test]
fn await_step_receives_a_signal_and_queries_read_partial_state() {
    let mut h = Harness::new();
    let ir = compile(APPROVAL);
    assert_eq!(ir.ir_version, "orbit-ir/2");
    let id = h.start(&ir, json!({ "n": 1 }));
    assert_eq!(h.state(&id).units["wait"].status(), UnitStatus::Waiting);
    // Before anything ran, every step reads as null.
    assert_eq!(
        h.engine.query(&id, "progress").unwrap(),
        json!({ "approved": null, "by": null, "n": null })
    );
    h.engine
        .signal(&id, "approve", json!({ "approved": true }))
        .unwrap();
    // Defaults from the signal declaration are applied.
    assert_eq!(
        h.state(&id).units["wait"].output,
        Some(json!({ "approved": true, "by": "nobody" }))
    );
    let w = h.worker("w", "python", &["a", "b"]);
    echo(&mut h, &w);
    echo(&mut h, &w);
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Completed);
    assert_eq!(s.output, Some(json!({ "approved": true, "by": "nobody" })));
    assert_eq!(
        h.engine.query(&id, "progress").unwrap(),
        json!({ "approved": true, "by": "nobody", "n": 2 })
    );
    let ev = h.events(&id);
    let pos = |n: &str| ev.iter().position(|e| *e == n).unwrap();
    assert!(pos("SignalAwaitStarted") < pos("SignalReceived"));
    assert!(pos("SignalReceived") < pos("SignalDelivered"));
}

#[test]
fn signals_sent_early_are_buffered_and_delivered_in_order() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0"
        signal note { text: String }
        step first { worker "a" with { n: 1 } }
        step one { await note after first timeout 1h }
        step two { await note after one timeout 1h }
        return { a: one.text, b: two.text }
    }"#,
    );
    let id = h.start(&ir, json!({}));
    h.engine
        .signal(&id, "note", json!({ "text": "x" }))
        .unwrap();
    h.engine
        .signal(&id, "note", json!({ "text": "y" }))
        .unwrap();
    assert_eq!(h.state(&id).signals.len(), 2);
    let w = h.worker("w", "python", &["a"]);
    echo(&mut h, &w);
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Completed);
    assert_eq!(s.output, Some(json!({ "a": "x", "b": "y" })));
    assert!(s.signals.iter().all(|r| r.delivered_to.is_some()));
}

#[test]
fn signal_timeout_fails_the_step_and_compensates() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0"
        signal approve { ok: Bool }
        step work { worker "a" with { n: 1 } compensate { worker "undo" with { n: work.n } } }
        step wait { await approve after work timeout 10m }
    }"#,
    );
    let id = h.start(&ir, json!({}));
    let w = h.worker("w", "python", &["a", "undo"]);
    echo(&mut h, &w);
    h.advance(10 * 60_000);
    let s = h.state(&id);
    assert_eq!(s.units["wait"].status(), UnitStatus::Failed);
    assert_eq!(
        s.units["wait"].error.as_ref().unwrap().code,
        "SIGNAL_TIMEOUT"
    );
    assert_eq!(s.status, WorkflowStatus::Compensating);
    // The worker's session expired during the silent ten minutes; register again.
    let w = h.worker("w", "python", &["a", "undo"]);
    let t = h.poll(&w).expect("compensation");
    assert_eq!(t.task_name, "undo");
}

#[test]
fn invalid_or_unknown_signals_and_queries_are_rejected() {
    let mut h = Harness::new();
    let ir = compile(APPROVAL);
    let id = h.start(&ir, json!({ "n": 1 }));
    let e = h
        .engine
        .signal(&id, "approve", json!({ "approved": "yes" }))
        .unwrap_err();
    assert_eq!((e.status, e.code), (422, "SIGNAL_PAYLOAD_INVALID"));
    let e = h.engine.signal(&id, "nope", json!({})).unwrap_err();
    assert_eq!((e.status, e.code), (404, "UNKNOWN_SIGNAL"));
    let e = h.engine.query(&id, "nope").unwrap_err();
    assert_eq!((e.status, e.code), (404, "UNKNOWN_QUERY"));
    h.engine.cancel(&id, "test").unwrap();
    let e = h
        .engine
        .signal(&id, "approve", json!({ "approved": true }))
        .unwrap_err();
    assert_eq!(e.status, 409);
    // A cancelled execution stops waiting: the await unit is withdrawn.
    assert_eq!(h.state(&id).units["wait"].status(), UnitStatus::Cancelled);
}

#[test]
fn signals_survive_restart() {
    let mut h = Harness::on_disk("signals");
    let ir = compile(APPROVAL);
    let id = h.start(&ir, json!({ "n": 5 }));
    h.engine
        .signal(&id, "approve", json!({ "approved": false, "by": "kim" }))
        .unwrap();
    let h = h.restart();
    let s = h.state(&id);
    assert_eq!(
        s.units["wait"].output,
        Some(json!({ "approved": false, "by": "kim" }))
    );
    assert_eq!(s.units["work"].status(), UnitStatus::Skipped);
}

// ---------------------------------------------------------------- compute

#[test]
fn compute_steps_run_in_the_runtime_including_fan_out() {
    let mut h = Harness::new();
    let ir = compile(
        r#"schema Item { price: Float, qty: Int }
        workflow W { version "1.0.0"
        input { items: List<Item> }
        step totals { compute { total: sum(map(input.items, i => i.price * float(i.qty))), big: any(input.items, i => i.price > 50.0) } }
        step each { for item in input.items compute item.qty * 2 }
        step charge { worker "a" with { n: int(totals.total) } }
        return { total: totals.total, big: totals.big, doubled: each, first: find(input.items, i => i.qty > 1) }
    }"#,
    );
    let id = h.start(
        &ir,
        json!({ "items": [{ "price": 10.0, "qty": 1 }, { "price": 60.0, "qty": 2 }] }),
    );
    let w = h.worker("w", "python", &["a"]);
    let t = echo(&mut h, &w);
    assert_eq!(t.input, json!({ "n": 130 }));
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Completed);
    assert_eq!(
        s.output,
        Some(
            json!({ "total": 130.0, "big": true, "doubled": [2, 4], "first": { "price": 60.0, "qty": 2 } })
        )
    );
}

#[test]
fn compute_errors_fail_the_step() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0" input { s: String } step parse { compute int(input.s) } }"#,
    );
    let id = h.start(&ir, json!({ "s": "not a number" }));
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Failed);
    assert_eq!(
        s.units["parse"].error.as_ref().unwrap().code,
        "EXPRESSION_ERROR"
    );
}

#[test]
fn v1_workflows_keep_their_ir_version() {
    let ir = compile(r#"workflow W { version "1.0.0" step s { worker "a" with { n: 1 } } }"#);
    assert_eq!(ir.ir_version, "orbit-ir/1");
    assert!(ir.signals.is_empty() && ir.queries.is_empty());
}

#[test]
fn a_failed_compensation_does_not_stop_the_others() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0"
        step one { worker "a" with { n: 1 } compensate { worker "undo" with { n: one.n } } }
        step two { worker "a" after one with { n: 2 } compensate { worker "undo" with { n: two.n } } }
        step boom { worker "b" after two with { n: 3 } }
    }"#,
    );
    let id = h.start(&ir, json!({}));
    let w = h.worker("w", "python", &["a", "b", "undo"]);
    echo(&mut h, &w);
    echo(&mut h, &w);
    let t = h.poll(&w).unwrap();
    h.engine
        .fail(&t.token, TaskError::new("BOOM", "no", false))
        .unwrap();
    // Newest first: two's compensation fails...
    let t = h.poll(&w).unwrap();
    assert_eq!(t.input, json!({ "n": 3 }));
    h.engine
        .fail(
            &t.token,
            TaskError::new("UNDO_FAILED", "cannot undo", false),
        )
        .unwrap();
    // ...and one's compensation still runs.
    let t = h.poll(&w).expect("remaining compensation still runs");
    assert_eq!(t.input, json!({ "n": 2 }));
    h.engine.complete(&t.token, json!({ "ok": true })).unwrap();
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Failed);
    assert_eq!(s.compensation, Some(CompensationOutcome::Failed));
    assert_eq!(s.units["compensate:one"].status(), UnitStatus::Succeeded);
    assert!(h.events(&id).contains(&"CompensationFailed"));
}

// ---------------------------------------------------------------- handlers

const WITH_HANDLERS: &str = r#"workflow W { version "1.0.0"
    input { n: Int }
    step one { worker "a" with { n: input.n } compensate { worker "undo" with { n: one.n } } }
    step boom { worker "b" after one with { n: 0 } }
    on failure {
        step report { compute { why: failure.code, msg: failure.message, reason: failure.reason, had_one: !is_null(one) } }
        step alert { worker "a" with { n: 100 } }
        step after_alert { worker "a" after alert with { n: alert.n } }
    }
    on cancel {
        step note { compute failure.reason }
    }
}"#;

#[test]
fn on_failure_handlers_run_after_compensation() {
    let mut h = Harness::new();
    let ir = compile(WITH_HANDLERS);
    assert_eq!(ir.ir_version, "orbit-ir/2");
    let id = h.start(&ir, json!({ "n": 1 }));
    let w = h.worker("w", "python", &["a", "b", "undo"]);
    echo(&mut h, &w); // one
    let t = h.poll(&w).unwrap();
    h.engine
        .fail(&t.token, TaskError::new("BOOM", "it broke", false))
        .unwrap();
    // Compensation first...
    let t = h.poll(&w).unwrap();
    assert_eq!(t.task_name, "undo");
    h.engine.complete(&t.token, json!({ "ok": true })).unwrap();
    // ...then the handler steps, still while the execution is not finished.
    assert!(!h.state(&id).status.is_terminal());
    let t = echo(&mut h, &w);
    assert_eq!(t.input, json!({ "n": 100 }));
    let t = echo(&mut h, &w);
    assert_eq!(t.input, json!({ "n": 101 }));
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Failed);
    assert_eq!(s.compensation, Some(CompensationOutcome::Completed));
    assert_eq!(s.error.as_ref().unwrap().code, "BOOM");
    assert_eq!(
        s.units["on_failure:report"].output,
        Some(
            json!({ "why": "BOOM", "msg": "step `boom` failed: it broke", "reason": "failed", "had_one": true })
        )
    );
    let ev = h.events(&id);
    let pos = |n: &str| ev.iter().position(|e| *e == n).unwrap();
    assert!(pos("CompensationCompleted") < pos("HandlersStarted"));
    assert!(pos("HandlersStarted") < pos("WorkflowFailed"));
}

#[test]
fn a_failing_handler_does_not_change_the_outcome() {
    let mut h = Harness::new();
    let ir = compile(WITH_HANDLERS);
    let id = h.start(&ir, json!({ "n": 1 }));
    let w = h.worker("w", "python", &["a", "b", "undo"]);
    echo(&mut h, &w);
    let t = h.poll(&w).unwrap();
    h.engine
        .fail(&t.token, TaskError::new("BOOM", "x", false))
        .unwrap();
    let t = h.poll(&w).unwrap();
    h.engine.complete(&t.token, json!({ "ok": true })).unwrap();
    let t = h.poll(&w).unwrap(); // alert
    h.engine
        .fail(&t.token, TaskError::new("PAGER_DOWN", "x", false))
        .unwrap();
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Failed);
    assert_eq!(
        s.error.as_ref().unwrap().code,
        "BOOM",
        "the handler's failure does not replace the cause"
    );
    assert_eq!(s.units["on_failure:alert"].status(), UnitStatus::Failed);
    assert_eq!(
        s.units["on_failure:after_alert"].status(),
        UnitStatus::Cancelled
    );
}

#[test]
fn on_cancel_runs_for_cancellations_and_no_handler_means_no_change() {
    let mut h = Harness::new();
    let ir = compile(WITH_HANDLERS);
    let id = h.start(&ir, json!({ "n": 1 }));
    h.engine.cancel(&id, "customer asked").unwrap();
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Cancelled);
    assert_eq!(s.units["on_cancel:note"].output, Some(json!("cancelled")));
    // A workflow timeout has no `on timeout` block here: it finishes directly.
    let ir2 = compile(
        r#"workflow T { version "1.0.0" timeout 1m step s { worker "a" with { n: 1 } }
           on failure { step x { compute 1 } } }"#,
    );
    let id2 = h.start(&ir2, json!({}));
    h.advance(60_000);
    let s2 = h.state(&id2);
    assert_eq!(s2.status, WorkflowStatus::TimedOut);
    assert!(!h.events(&id2).contains(&"HandlersStarted"));
}

#[test]
fn handlers_read_steps_that_never_ran_as_null() {
    let mut h = Harness::new();
    let ir = compile(
        r#"workflow W { version "1.0.0"
        step first { worker "b" with { n: 1 } }
        step second { worker "a" after first with { n: 2 } }
        on failure { step summary { compute { second_ran: !is_null(second), n: coalesce(second.n, -1) } } }
    }"#,
    );
    let id = h.start(&ir, json!({}));
    let w = h.worker("w", "python", &["a", "b"]);
    let t = h.poll(&w).unwrap();
    h.engine
        .fail(&t.token, TaskError::new("NOPE", "x", false))
        .unwrap();
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Failed);
    assert_eq!(
        s.units["on_failure:summary"].output,
        Some(json!({ "second_ran": false, "n": -1 }))
    );
}

// ---------------------------------------------------------------- limits

fn limited(limits: orbit_runtime::engine::Limits) -> Harness {
    let clock = Arc::new(AtomicU64::new(1_000_000));
    let c = clock.clone();
    let engine = Engine::open(
        Store::in_memory().unwrap(),
        EngineConfig {
            limits,
            ..EngineConfig::default()
        },
        Box::new(move || c.load(Ordering::SeqCst)),
    )
    .unwrap();
    Harness {
        engine,
        clock,
        db: None,
    }
}

#[test]
fn max_active_executions_rejects_new_starts_but_not_idempotent_ones() {
    let mut h = limited(orbit_runtime::engine::Limits {
        max_active_executions: Some(1),
        ..Default::default()
    });
    let ir = compile(r#"workflow W { version "1.0.0" step s { worker "a" with { n: 1 } } }"#);
    h.engine.register_definition(ir.clone()).unwrap();
    h.engine
        .start("W", None, json!({}), Some("one".into()))
        .unwrap();
    let e = h.engine.start("W", None, json!({}), None).unwrap_err();
    assert_eq!((e.status, e.code), (429, "RESOURCE_EXHAUSTED"));
    // Starting the same id again is still idempotent.
    assert!(
        !h.engine
            .start("W", None, json!({}), Some("one".into()))
            .unwrap()
            .created
    );
}

#[test]
fn max_fan_out_fails_the_step() {
    let mut h = limited(orbit_runtime::engine::Limits {
        max_fan_out: Some(2),
        ..Default::default()
    });
    let ir = compile(
        r#"workflow W { version "1.0.0" input { l: List<Int> } step each { for x in input.l compute x } }"#,
    );
    let id = h.start(&ir, json!({ "l": [1, 2, 3] }));
    let s = h.state(&id);
    assert_eq!(s.status, WorkflowStatus::Failed);
    assert_eq!(
        s.units["each"].error.as_ref().unwrap().code,
        "LIMIT_EXCEEDED"
    );
    let ok = h.start(&ir, json!({ "l": [1, 2] }));
    assert_eq!(h.state(&ok).status, WorkflowStatus::Completed);
}

#[test]
fn max_history_events_fails_the_execution_and_still_compensates() {
    let mut h = limited(orbit_runtime::engine::Limits {
        max_history_events: Some(8),
        ..Default::default()
    });
    let ir = compile(
        r#"workflow W { version "1.0.0"
        step one { worker "a" with { n: 1 } compensate { worker "undo" with { n: one.n } } }
        step two { worker "a" after one with { n: 2 } }
        step three { worker "a" after two with { n: 3 } }
    }"#,
    );
    let id = h.start(&ir, json!({}));
    let w = h.worker("w", "python", &["a", "undo"]);
    echo(&mut h, &w);
    echo(&mut h, &w);
    let s = h.state(&id);
    assert!(
        h.events(&id).contains(&"LimitExceeded"),
        "{:?}",
        h.events(&id)
    );
    // The compensation still runs past the limit.
    let t = h.poll(&w).expect("compensation");
    assert_eq!(t.task_name, "undo");
    h.engine.complete(&t.token, json!({ "ok": true })).unwrap();
    let s2 = h.state(&id);
    assert_eq!(s2.status, WorkflowStatus::Failed);
    assert_eq!(s2.error.as_ref().unwrap().code, "LIMIT_EXCEEDED");
    let _ = s;
}

#[test]
fn max_payload_bytes_rejects_large_inputs_results_and_signals() {
    let mut h = limited(orbit_runtime::engine::Limits {
        max_payload_bytes: Some(64),
        ..Default::default()
    });
    let ir = compile(
        r#"workflow W { version "1.0.0" input { s: String } signal go { s: String }
           step a { worker "a" with { n: 1 } } step w { await go after a timeout 1h } }"#,
    );
    h.engine.register_definition(ir.clone()).unwrap();
    let big = "x".repeat(100);
    let e = h
        .engine
        .start("W", None, json!({ "s": big }), None)
        .unwrap_err();
    assert_eq!((e.status, e.code), (413, "PAYLOAD_TOO_LARGE"));
    let id = h
        .engine
        .start("W", None, json!({ "s": "ok" }), None)
        .unwrap()
        .execution_id;
    let w = h.worker("w", "python", &["a"]);
    let t = h.poll(&w).unwrap();
    let e = h
        .engine
        .complete(&t.token, json!({ "n": 1, "pad": big }))
        .unwrap_err();
    assert_eq!(e.code, "PAYLOAD_TOO_LARGE");
    let e = h.engine.signal(&id, "go", json!({ "s": big })).unwrap_err();
    assert_eq!(e.code, "PAYLOAD_TOO_LARGE");
}
