//! The engine on the PostgreSQL backend.
//!
//! Runs only when `ORBIT_TEST_POSTGRES_URL` points at a server where the user
//! may create databases, e.g. `postgres://orbit@127.0.0.1:5432/postgres`. Each
//! test creates (and drops) its own database.

use orbit_ir::WorkflowIr;
use orbit_runtime::engine::{ContractAnnouncement, RegisterWorker};
use orbit_runtime::model::{
    CompensationOutcome, ExecutionState, TaskError, UnitStatus, WorkflowStatus,
};
use orbit_runtime::{Engine, EngineConfig, Store};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const SRC: &str = r#"
task "a" { version "1.0.0" input { n: Int } output { n: Int } }
task "undo" { version "1.0.0" input { n: Int } output { ok: Bool } }
workflow Child { version "1.0.0" input { n: Int } output { n: Int }
    step c { worker "a" with { n: input.n } } return { n: c.n } }
workflow Main {
    version "1.0.0"
    input { ns: List<Int> }
    step each {
        for n in input.ns
        worker "a"
        with { n: n }
        retry { attempts: 2, initial_delay: 1s }
        compensate { worker "undo" with { n: each.n } }
    }
    step child { workflow "Child" after each with { n: len(each) } }
    step last { worker "a" after child with { n: child.n } }
}
"#;

struct Db {
    admin: String,
    name: String,
    url: String,
}

impl Db {
    fn create(test: &str) -> Option<Db> {
        let full = std::env::var("ORBIT_TEST_POSTGRES_URL").ok()?;
        // Keep query parameters such as `?sslmode=require` for the engine's URL;
        // the fixture's own admin connection does not need them.
        let (admin, query) = match full.split_once('?') {
            Some((a, q)) => (a.to_string(), format!("?{q}")),
            None => (full.clone(), String::new()),
        };
        let name = format!("orbit_test_{}_{test}", std::process::id());
        let mut c = postgres::Client::connect(&admin, postgres::NoTls)
            .expect("connect to ORBIT_TEST_POSTGRES_URL");
        c.batch_execute(&format!("DROP DATABASE IF EXISTS {name}"))
            .unwrap();
        c.batch_execute(&format!("CREATE DATABASE {name}")).unwrap();
        let base = admin.rsplit_once('/').map(|(b, _)| b).unwrap_or(&admin);
        let url = format!("{base}/{name}{query}");
        Some(Db { admin, name, url })
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Ok(mut c) = postgres::Client::connect(&self.admin, postgres::NoTls) {
            let _ = c.batch_execute(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                self.name
            ));
        }
    }
}

fn engine(url: &str, clock: &Arc<AtomicU64>) -> Engine {
    let c = clock.clone();
    Engine::open(
        Store::connect(url).unwrap(),
        EngineConfig::default(),
        Box::new(move || c.load(Ordering::SeqCst)),
    )
    .unwrap()
}

fn workflows() -> Vec<WorkflowIr> {
    let c = orbit_compiler::compile_str("pg.orbit", SRC);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    c.workflows
}

fn worker(e: &mut Engine) -> String {
    e.register_worker(RegisterWorker {
        worker_id: "pg-worker".into(),
        runtime: "rust".into(),
        sdk: None,
        contracts: ["a", "undo"]
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

macro_rules! db_or_skip {
    ($name:literal) => {
        match Db::create($name) {
            Some(db) => db,
            None => {
                eprintln!("skipping: set ORBIT_TEST_POSTGRES_URL to run PostgreSQL tests");
                return;
            }
        }
    };
}

#[test]
fn full_lifecycle_survives_restarts_on_postgres() {
    let db = db_or_skip!("lifecycle");
    let clock = Arc::new(AtomicU64::new(1_000_000));
    let mut e = engine(&db.url, &clock);
    for wf in workflows() {
        assert!(e.register_definition(wf.clone()).unwrap());
        assert!(
            !e.register_definition(wf).unwrap(),
            "idempotent re-registration"
        );
    }
    let id = e
        .start(
            "Main",
            None,
            json!({ "ns": [1, 2] }),
            Some("order-1".into()),
        )
        .unwrap()
        .execution_id;
    assert!(
        !e.start(
            "Main",
            None,
            json!({ "ns": [1, 2] }),
            Some("order-1".into())
        )
        .unwrap()
        .created
    );
    let w = worker(&mut e);

    // Element 0 fails once (retry), element 1 succeeds.
    let t0 = e.poll(&w).unwrap().unwrap();
    let t1 = e.poll(&w).unwrap().unwrap();
    e.fail(&t0.token, TaskError::new("FLAKY", "try again", true))
        .unwrap();
    e.complete(&t1.token, json!({ "n": 20 })).unwrap();
    let before = serde_json::to_value(e.execution(&id).unwrap()).unwrap();

    // Crash: the state is rebuilt from PostgreSQL alone.
    drop(e);
    let mut e = engine(&db.url, &clock);
    assert_eq!(
        serde_json::to_value(e.execution(&id).unwrap()).unwrap(),
        before
    );
    clock.fetch_add(1_000, Ordering::SeqCst);
    e.tick().unwrap();
    let w = worker(&mut e);
    let retry = e.poll(&w).unwrap().unwrap();
    assert_eq!((retry.step_id.as_str(), retry.attempt), ("each[0]", 2));
    e.complete(&retry.token, json!({ "n": 10 })).unwrap();

    // The child workflow runs; its task fails permanently, so the parent
    // compensates both elements.
    e.tick().unwrap();
    let child_task = e.poll(&w).unwrap().unwrap();
    assert_eq!(child_task.execution_id, "order-1.child");
    e.fail(&child_task.token, TaskError::new("BROKEN", "no", false))
        .unwrap();
    e.tick().unwrap();
    let mut compensated = vec![];
    while let Some(t) = e.poll(&w).unwrap() {
        compensated.push(t.step_id.clone());
        e.complete(&t.token, json!({ "ok": true })).unwrap();
    }
    assert_eq!(
        compensated,
        vec!["compensate:each[0]", "compensate:each[1]"]
    );
    let s = e.execution(&id).unwrap();
    assert_eq!(s.status, WorkflowStatus::Failed);
    assert_eq!(s.compensation, Some(CompensationOutcome::Completed));
    assert_eq!(s.units["each"].status(), UnitStatus::Succeeded);

    // History is authoritative and gap-free; listings and counts work.
    let history = e.history(&id).unwrap();
    assert!(history
        .iter()
        .enumerate()
        .all(|(i, h)| h.seq == i as u64 + 1));
    let replayed = ExecutionState::replay(&id, &history);
    assert_eq!(
        serde_json::to_value(replayed).unwrap(),
        serde_json::to_value(&s).unwrap()
    );
    let listed = e.list_executions(Some("FAILED"), Some("Main"), 10).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(e
        .store_stats()
        .unwrap()
        .iter()
        .any(|(st, n)| st == "FAILED" && *n >= 1));
    assert_eq!(e.workers().unwrap()[0].record.worker_id, "pg-worker");
}

#[test]
fn definitions_are_immutable_on_postgres() {
    let db = db_or_skip!("immutable");
    let clock = Arc::new(AtomicU64::new(1));
    let mut e = engine(&db.url, &clock);
    let wfs = workflows();
    e.register_definition(wfs[0].clone()).unwrap();
    let mut changed = wfs[0].clone();
    changed.description = Some("different".into());
    let changed = changed.seal();
    assert_eq!(
        e.register_definition(changed).unwrap_err().code,
        "DEFINITION_CONFLICT"
    );
}
