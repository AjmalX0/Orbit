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
