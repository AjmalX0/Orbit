//! `continue_as_new`: runs, per-run histories, signals carried over, crash
//! recovery, children, idempotency keys, limits and replay.

use orbit_ir::WorkflowIr;
use orbit_runtime::engine::{ContractAnnouncement, RegisterWorker, TaskAssignment};
use orbit_runtime::model::{Event, WorkflowStatus};
use orbit_runtime::{Engine, EngineConfig, Store};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const SRC: &str = r#"
task "page" { version "1.0.0" input { cursor: Int } output { next: Int, done: Bool } }

workflow Poller {
    version "1.0.0"
    input { cursor: Int = 0, pages: Int = 0 }
    output { pages: Int, cursor: Int }
    step fetch { worker "page" with { cursor: input.cursor } }
    continue_as_new when !fetch.done with { cursor: fetch.next, pages: input.pages + 1 }
    return { pages: input.pages + 1, cursor: fetch.next }
}

workflow Subscription {
    version "1.0.0"
    input { month: Int = 1 }
    output { months: Int }
    signal renew { months: Int }
    step wait { await renew }
    step bill { worker "page" with { cursor: input.month } }
    continue_as_new when wait.months > 0 with { month: input.month + 1 }
    return { months: input.month }
}

workflow Child {
    version "1.0.0"
    input { cursor: Int }
    output { next: Int }
    step fetch { worker "page" with { cursor: input.cursor } }
    return { next: fetch.next }
}

workflow Parent {
    version "1.0.0"
    input { round: Int = 1 }
    step child { workflow "Child" with { cursor: input.round } }
    continue_as_new when input.round < 3 with { round: input.round + 1 }
}

workflow Wrapper {
    version "1.0.0"
    output { pages: Int }
    step poll { workflow "Poller" with {} }
    return { pages: poll.pages }
}
"#;

fn compile() -> Vec<WorkflowIr> {
    let c = orbit_compiler::compile_str("can.orbit", SRC);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    c.workflows
}

struct Harness {
    engine: Engine,
    clock: Arc<AtomicU64>,
    db: PathBuf,
    session: String,
}

impl Harness {
    fn new(name: &str, config: EngineConfig) -> Self {
        let dir = std::env::temp_dir().join(format!("orbit-can-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("orbit.db");
        let _ = std::fs::remove_file(&db);
        let mut h = Self::open(db, Arc::new(AtomicU64::new(1_000_000)), config);
        let wfs = compile();
        // Children first.
        for name in ["Child", "Poller", "Subscription", "Parent", "Wrapper"] {
            let wf = wfs.iter().find(|w| w.name == name).unwrap();
            h.engine.register_definition(wf.clone()).unwrap();
        }
        h
    }

    fn open(db: PathBuf, clock: Arc<AtomicU64>, config: EngineConfig) -> Self {
        let c = clock.clone();
        let mut engine = Engine::open(
            Store::open(&db).unwrap(),
            config,
            Box::new(move || c.load(Ordering::SeqCst)),
        )
        .unwrap();
        let session = engine
            .register_worker(RegisterWorker {
                worker_id: "w".into(),
                runtime: "python".into(),
                sdk: None,
                contracts: vec![ContractAnnouncement {
                    task_name: "page".into(),
                    contract_version: "1.0.0".into(),
                }],
            })
            .unwrap()
            .worker_session_id;
        Harness {
            engine,
            clock,
            db,
            session,
        }
    }

    fn restart(self) -> Self {
        let (db, clock, config) = (
            self.db.clone(),
            self.clock.clone(),
            self.engine.config.clone(),
        );
        drop(self.engine);
        Self::open(db, clock, config)
    }

    fn tick(&mut self) {
        self.clock.fetch_add(10, Ordering::SeqCst);
        self.engine.tick().unwrap();
    }

    fn poll(&mut self) -> Option<TaskAssignment> {
        self.engine.poll(&self.session).unwrap()
    }

    /// Answer `page` tasks: `done` once the cursor reaches `last`.
    fn pages(&mut self, last: i64) -> Vec<TaskAssignment> {
        let mut served = vec![];
        let mut idle = 0;
        while idle < 3 && served.len() < 100 {
            self.tick();
            match self.poll() {
                Some(t) => {
                    idle = 0;
                    let c = t.input["cursor"].as_i64().unwrap();
                    self.engine
                        .complete(&t.token, json!({ "next": c + 10, "done": c >= last }))
                        .unwrap();
                    served.push(t);
                }
                None => idle += 1,
            }
        }
        served
    }

    /// Answer one `page` task with "not done", so a second run starts.
    fn one_page(&mut self) {
        self.tick();
        let t = self.poll().expect("a page task");
        self.engine
            .complete(&t.token, json!({ "next": 5, "done": false }))
            .unwrap();
        self.tick();
    }
}

#[test]
fn each_run_starts_fresh_and_the_last_one_completes() {
    let mut h = Harness::new("basic", EngineConfig::default());
    let id = h
        .engine
        .start("Poller", None, json!({}), Some("poll-1".into()))
        .unwrap()
        .execution_id;
    let served = h.pages(20);
    assert_eq!(served.len(), 3);
    // Every run's task has its own idempotency key.
    let keys: Vec<&str> = served.iter().map(|t| t.idempotency_key.as_str()).collect();
    assert_eq!(
        keys,
        ["poll-1/fetch", "poll-1/run2/fetch", "poll-1/run3/fetch"]
    );
    let s = h.engine.execution(&id).unwrap();
    assert_eq!(s.status, WorkflowStatus::Completed);
    assert_eq!(s.run, 3);
    assert_eq!(s.output, Some(json!({ "pages": 3, "cursor": 30 })));
    assert_eq!(s.input, json!({ "cursor": 20, "pages": 2 }));
    // The latest run's history starts again at 1.
    let h3 = h.engine.history(&id).unwrap();
    assert_eq!(h3[0].seq, 1);
    assert!(matches!(
        &h3[0].event,
        Event::WorkflowCreated { run: Some(3), .. }
    ));
    // Earlier runs are archived and end with WorkflowContinuedAsNew.
    for run in [1u32, 2] {
        let hist = h.engine.run_history(&id, run).unwrap();
        assert_eq!(hist[0].seq, 1);
        match &hist.last().unwrap().event {
            Event::WorkflowContinuedAsNew { next_run, input } => {
                assert_eq!(*next_run, run + 1);
                assert_eq!(input["pages"], json!(run));
            }
            other => panic!("run {run} ended with {other:?}"),
        }
        let state = orbit_runtime::model::ExecutionState::replay(&id, &hist);
        assert_eq!(state.status, WorkflowStatus::ContinuedAsNew);
        assert_eq!(state.run, run);
    }
    assert_eq!(h.engine.run_history(&id, 3).unwrap().len(), h3.len());
    assert_eq!(
        h.engine.run_history(&id, 4).unwrap_err().code,
        "RUN_NOT_FOUND"
    );
    // One execution, one row.
    assert_eq!(h.engine.list_executions(None, None, 10).unwrap().len(), 1);
    assert_eq!(h.engine.metrics.executions_started, 1);
    assert_eq!(h.engine.metrics.executions_continued_as_new, 2);
    assert_eq!(h.engine.metrics.executions_completed, 1);
}

#[test]
fn every_run_replays_consistently() {
    let mut h = Harness::new("replay", EngineConfig::default());
    h.engine
        .start("Poller", None, json!({}), Some("p".into()))
        .unwrap();
    h.pages(10);
    let def = h.engine.definition("Poller", None).unwrap();
    for run in 1..=2 {
        let hist = h.engine.run_history("p", run).unwrap();
        let r = orbit_runtime::replay::replay("p", &hist, &def, None);
        assert!(r.consistent(), "run {run}: {:#?}", r.divergences);
        assert!(r.pending.is_empty());
    }
}

#[test]
fn signals_sent_during_the_switch_reach_the_new_run() {
    let mut h = Harness::new("signals", EngineConfig::default());
    h.engine
        .start("Subscription", None, json!({}), Some("sub".into()))
        .unwrap();
    // Two renewals arrive at once: run 1 takes the first, the second is
    // still buffered when run 1 continues as new.
    h.engine
        .signal("sub", "renew", json!({ "months": 1 }))
        .unwrap();
    h.engine
        .signal("sub", "renew", json!({ "months": 1 }))
        .unwrap();
    h.tick();
    let t = h.poll().expect("bill of run 1");
    assert_eq!(t.input, json!({ "cursor": 1 }));
    // A third signal arrives while run 1's last step is in flight.
    h.engine
        .signal("sub", "renew", json!({ "months": 0 }))
        .unwrap();
    h.engine
        .complete(&t.token, json!({ "next": 0, "done": true }))
        .unwrap();
    h.tick();
    let s = h.engine.execution("sub").unwrap();
    assert_eq!(s.run, 2);
    // Both undelivered signals were carried over in arrival order; the
    // `await` of run 2 took the first of them.
    let hist = h.engine.history("sub").unwrap();
    let carried: Vec<&Value> = hist
        .iter()
        .filter_map(|e| match &e.event {
            Event::SignalReceived { payload, .. } => Some(payload),
            _ => None,
        })
        .collect();
    assert_eq!(carried, [&json!({ "months": 1 }), &json!({ "months": 0 })]);
    assert_eq!(s.units["wait"].output, Some(json!({ "months": 1 })));
    let t = h.poll().expect("bill of run 2");
    assert_eq!(t.input, json!({ "cursor": 2 }));
    h.engine
        .complete(&t.token, json!({ "next": 0, "done": true }))
        .unwrap();
    h.tick();
    // Run 3 takes `months: 0` and completes instead of continuing.
    let t = h.poll().expect("bill of run 3");
    h.engine
        .complete(&t.token, json!({ "next": 0, "done": true }))
        .unwrap();
    h.tick();
    let s = h.engine.execution("sub").unwrap();
    assert_eq!((s.run, s.status), (3, WorkflowStatus::Completed));
    assert_eq!(s.output, Some(json!({ "months": 3 })));
    // Queries and signals address the current run.
    let e = h
        .engine
        .signal("sub", "renew", json!({ "months": 1 }))
        .unwrap_err();
    assert_eq!(e.code, "EXECUTION_FINISHED");
}

#[test]
fn a_crash_after_the_switch_resumes_the_new_run() {
    let mut h = Harness::new("crash", EngineConfig::default());
    h.engine
        .start("Poller", None, json!({}), Some("c".into()))
        .unwrap();
    h.tick();
    let t = h.poll().unwrap();
    h.engine
        .complete(&t.token, json!({ "next": 5, "done": false }))
        .unwrap();
    // Run 2 is created and its first task queued in the same commit series.
    let s = h.engine.execution("c").unwrap();
    assert_eq!(s.run, 2);
    let mut h = h.restart();
    let s = h.engine.execution("c").unwrap();
    assert_eq!((s.run, s.status), (2, WorkflowStatus::Running));
    assert_eq!(s.input, json!({ "cursor": 5, "pages": 1 }));
    // The recovered run carries on; run 1 stays archived.
    h.pages(5);
    let s = h.engine.execution("c").unwrap();
    assert_eq!((s.run, s.status), (2, WorkflowStatus::Completed));
    // Created, Started, Scheduled, Dispatched, Succeeded, ContinuedAsNew.
    assert_eq!(h.engine.run_history("c", 1).unwrap().len(), 6);
}

#[test]
fn a_run_crashing_mid_step_is_recovered_with_its_run_number() {
    let mut h = Harness::new("crash-mid", EngineConfig::default());
    h.engine
        .start("Poller", None, json!({}), Some("m".into()))
        .unwrap();
    h.one_page(); // run 1 done, run 2's task queued but no worker answers yet
    let before = h.engine.execution("m").unwrap();
    let h = h.restart();
    let after = h.engine.execution("m").unwrap();
    assert_eq!(before.run, after.run);
    assert_eq!(after.status, WorkflowStatus::Running);
}

#[test]
fn children_of_later_runs_are_new_executions() {
    let mut h = Harness::new("children", EngineConfig::default());
    h.engine
        .start("Parent", None, json!({}), Some("par".into()))
        .unwrap();
    h.pages(100);
    let s = h.engine.execution("par").unwrap();
    assert_eq!((s.run, s.status), (3, WorkflowStatus::Completed));
    let mut ids: Vec<String> = h
        .engine
        .list_executions(None, None, 10)
        .unwrap()
        .into_iter()
        .map(|e| e.execution_id)
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        ["par", "par.child", "par.run2.child", "par.run3.child"]
    );
    let child = h.engine.execution("par.run3.child").unwrap();
    assert_eq!(child.input, json!({ "cursor": 3 }));
}

#[test]
fn a_parent_waits_for_the_final_run_of_its_child() {
    let mut h = Harness::new("child-runs", EngineConfig::default());
    h.engine
        .start("Wrapper", None, json!({}), Some("wrap".into()))
        .unwrap();
    h.pages(20);
    let s = h.engine.execution("wrap").unwrap();
    assert_eq!(s.status, WorkflowStatus::Completed);
    assert_eq!(s.output, Some(json!({ "pages": 3 })));
    let child = h.engine.execution("wrap.poll").unwrap();
    assert_eq!((child.run, child.status), (3, WorkflowStatus::Completed));
    assert_eq!(child.parent.unwrap().execution_id, "wrap");
}

#[test]
fn max_history_events_applies_per_run() {
    // Each run of Poller records 7 events; three runs need 21.
    let config = EngineConfig {
        limits: orbit_runtime::engine::Limits {
            max_history_events: Some(10),
            ..Default::default()
        },
        ..EngineConfig::default()
    };
    let mut h = Harness::new("limits", config);
    h.engine
        .start("Poller", None, json!({}), Some("l".into()))
        .unwrap();
    h.pages(20);
    let s = h.engine.execution("l").unwrap();
    assert_eq!((s.run, s.status), (3, WorkflowStatus::Completed));
}

#[test]
fn cancelling_the_execution_cancels_the_current_run() {
    let mut h = Harness::new("cancel", EngineConfig::default());
    h.engine
        .start("Poller", None, json!({}), Some("x".into()))
        .unwrap();
    h.one_page();
    h.engine.cancel("x", "enough").unwrap();
    h.tick();
    let s = h.engine.execution("x").unwrap();
    assert_eq!((s.run, s.status), (2, WorkflowStatus::Cancelled));
    assert_eq!(h.engine.active_count(), 0);
}

#[test]
fn an_invalid_next_input_fails_the_run() {
    let src = r#"
task "page" { version "1.0.0" input { cursor: Int } output { next: Int, done: Bool } }
workflow Bad {
    version "1.0.0"
    input { n: Int = 0 }
    step s { compute { n: input.n } }
    continue_as_new with { n: 10 / input.n }
}
"#;
    let c = orbit_compiler::compile_str("bad.orbit", src);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    let mut h = Harness::new("bad-input", EngineConfig::default());
    h.engine
        .register_definition(c.workflows[0].clone())
        .unwrap();
    h.engine
        .start("Bad", None, json!({}), Some("b".into()))
        .unwrap();
    h.tick();
    let s = h.engine.execution("b").unwrap();
    assert_eq!((s.run, s.status), (1, WorkflowStatus::Failed));
    assert_eq!(s.error.unwrap().code, "EXPRESSION_ERROR");
}
