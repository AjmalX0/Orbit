//! Store-level tests of run archives (continue-as-new) and schedules, on
//! SQLite and, when `ORBIT_TEST_POSTGRES_URL` is set, on PostgreSQL (each
//! test then creates and drops its own database).

use orbit_runtime::engine::{ContractAnnouncement, RegisterWorker};
use orbit_runtime::model::{Event, ExecutionState, HistoryEvent, WorkflowStatus};
use orbit_runtime::schedule::CreateSchedule;
use orbit_runtime::store::{FireRecord, Rollover, ScheduleRow};
use orbit_runtime::{Engine, EngineConfig, Store};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

struct Db {
    admin: String,
    name: String,
    url: String,
}

impl Db {
    fn create(test: &str) -> Option<Db> {
        let full = std::env::var("ORBIT_TEST_POSTGRES_URL").ok()?;
        let (admin, query) = match full.split_once('?') {
            Some((a, q)) => (a.to_string(), format!("?{q}")),
            None => (full.clone(), String::new()),
        };
        let name = format!("orbit_test_{}_{test}", std::process::id());
        let mut c = postgres::Client::connect(&admin, postgres::NoTls).expect("connect");
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

/// Run `f` with a location on SQLite, then on PostgreSQL if configured.
fn on_both(test: &str, f: impl Fn(&str)) {
    let dir = std::env::temp_dir().join(format!("orbit-store-{}-{test}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("orbit.db");
    let _ = std::fs::remove_file(&file);
    f(file.to_str().unwrap());
    if let Some(db) = Db::create(test) {
        f(&db.url);
    }
}

fn ev(seq: u64, event: Event) -> HistoryEvent {
    HistoryEvent {
        seq,
        timestamp_ms: 1000 + seq,
        event,
    }
}

fn created(run: Option<u32>) -> Event {
    Event::WorkflowCreated {
        workflow: "W".into(),
        version: "1.0.0".into(),
        definition_hash: "sha256:x".into(),
        input: json!({ "run": run }),
        parent: None,
        schedule_id: Some("nightly".into()),
        run,
    }
}

#[test]
fn rollover_archives_the_finished_run_atomically() {
    on_both("rollover", |loc| {
        let mut store = Store::connect(loc).unwrap();
        let run1 = vec![ev(1, created(None)), ev(2, Event::WorkflowStarted {})];
        let s1 = ExecutionState::replay("e", &run1);
        store.append(&s1, &run1).unwrap();
        let tail = vec![ev(
            3,
            Event::WorkflowContinuedAsNew {
                next_run: 2,
                input: json!({ "n": 2 }),
            },
        )];
        let run2 = vec![
            ev(1, created(Some(2))),
            ev(2, Event::WorkflowStarted {}),
            ev(
                3,
                Event::SignalReceived {
                    name: "go".into(),
                    payload: json!({}),
                },
            ),
        ];
        let s2 = ExecutionState::replay("e", &run2);
        store
            .append_rollover(
                &s2,
                &run2,
                Rollover {
                    run: 1,
                    events: &tail,
                },
            )
            .unwrap();
        // The live history is run 2's, starting at 1.
        let live = store.history("e").unwrap();
        assert_eq!(live.len(), 3);
        let state = ExecutionState::replay("e", &live);
        assert_eq!(state.run, 2);
        assert_eq!(state.schedule_id.as_deref(), Some("nightly"));
        assert_eq!(state.status, WorkflowStatus::Running);
        // Run 1 is archived whole, ending with its terminal event.
        let archived = store.run_history("e", 1).unwrap();
        assert_eq!(
            archived.iter().map(|h| h.seq).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        let r1 = ExecutionState::replay("e", &archived);
        assert_eq!(r1.status, WorkflowStatus::ContinuedAsNew);
        assert!(store.run_history("e", 2).unwrap().is_empty());
        // One projection row, pointing at the running run.
        assert_eq!(
            store.execution_status("e").unwrap().as_deref(),
            Some("RUNNING")
        );
        assert_eq!(store.active_execution_ids().unwrap(), ["e"]);
        // Opening the store again (schema creation is idempotent) keeps it all.
        drop(store);
        let store = Store::connect(loc).unwrap();
        assert_eq!(store.history("e").unwrap().len(), 3);
        assert_eq!(store.run_history("e", 1).unwrap().len(), 3);
    });
}

fn row(id: &str, next: u64) -> ScheduleRow {
    ScheduleRow {
        schedule_id: id.into(),
        spec: r#"{"workflow":"W","interval":"1m"}"#.into(),
        paused: false,
        next_fire_ms: Some(next),
        due_ms: Some(next + 5),
        last_fire_ms: None,
        last_execution_id: None,
        fired: 0,
        skipped: 0,
        last_error: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}

#[test]
fn schedule_rows_claims_and_compare_and_set() {
    on_both("sched", |loc| {
        let store = Store::connect(loc).unwrap();
        assert!(store.insert_schedule(&row("a", 60_000)).unwrap());
        assert!(!store.insert_schedule(&row("a", 1)).unwrap());
        assert!(store.insert_schedule(&row("b", 120_000)).unwrap());
        assert_eq!(store.schedules().unwrap().len(), 2);
        // Due uses due_ms (with jitter), not the nominal time.
        assert!(store.due_schedules(60_004).unwrap().is_empty());
        let due = store.due_schedules(60_005).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].schedule_id, "a");
        // Exactly one claimer wins the occurrence.
        assert!(store
            .claim_schedule("a", 60_000, "n1", 60_005, 90_005)
            .unwrap());
        assert!(!store
            .claim_schedule("a", 60_000, "n2", 60_006, 90_006)
            .unwrap());
        // A claimed schedule is not due for others while the claim lasts.
        assert!(store.due_schedules(70_000).unwrap().is_empty());
        // A stale occurrence cannot be claimed.
        assert!(!store
            .claim_schedule("b", 60_000, "n2", 130_000, 160_000)
            .unwrap());
        let rec = FireRecord {
            next_fire_ms: Some(120_000),
            due_ms: Some(120_000),
            last_fire_ms: Some(60_000),
            last_execution_id: Some("a:x".into()),
            fired: 1,
            skipped: 2,
            last_error: None,
        };
        // Only the claim holder, on the claimed occurrence, may advance.
        assert!(!store
            .finish_schedule_fire("a", 60_000, "n2", 60_010, &rec)
            .unwrap());
        assert!(store
            .finish_schedule_fire("a", 60_000, "n1", 60_010, &rec)
            .unwrap());
        assert!(!store
            .finish_schedule_fire("a", 60_000, "n1", 60_010, &rec)
            .unwrap());
        let a = store.schedule("a").unwrap().unwrap();
        assert_eq!(
            (
                a.next_fire_ms,
                a.fired,
                a.skipped,
                a.last_execution_id.as_deref()
            ),
            (Some(120_000), 1, 2, Some("a:x"))
        );
        // An expired claim can be taken over.
        assert!(store
            .claim_schedule("a", 120_000, "n1", 120_000, 150_000)
            .unwrap());
        assert!(store
            .claim_schedule("a", 120_000, "n2", 150_000, 180_000)
            .unwrap());
        // Release with a retry time and an error.
        store
            .release_schedule_claim("a", "n2", 155_000, "boom", 150_001)
            .unwrap();
        assert!(store
            .due_schedules(154_999)
            .unwrap()
            .iter()
            .all(|r| r.schedule_id != "a"));
        let a = store.schedule("a").unwrap().unwrap();
        assert_eq!(a.last_error.as_deref(), Some("boom"));
        store
            .record_schedule_trigger("a", "a:manual:x", 160_000)
            .unwrap();
        assert_eq!(store.schedule("a").unwrap().unwrap().fired, 2);
        // Update replaces the spec and drops claims; paused ones are never due.
        let mut b = store.schedule("b").unwrap().unwrap();
        b.paused = true;
        assert!(store.update_schedule(&b).unwrap());
        assert!(store
            .due_schedules(1 << 40)
            .unwrap()
            .iter()
            .all(|r| r.schedule_id != "b"));
        assert!(store.delete_schedule("b").unwrap());
        assert!(!store.delete_schedule("b").unwrap());
        assert!(store.schedule("b").unwrap().is_none());
    });
}

const SRC: &str = r#"
task "page" { version "1.0.0" input { cursor: Int } output { next: Int, done: Bool } }
workflow Poller {
    version "1.0.0"
    input { cursor: Int = 0 }
    step fetch { worker "page" with { cursor: input.cursor } }
    continue_as_new when !fetch.done with { cursor: fetch.next }
}
"#;

fn open(loc: &str, clock: &Arc<AtomicU64>) -> (Engine, String) {
    let c = clock.clone();
    let mut e = Engine::open(
        Store::connect(loc).unwrap(),
        EngineConfig {
            session_timeout_ms: 1 << 40,
            ..EngineConfig::default()
        },
        Box::new(move || c.load(Ordering::SeqCst)),
    )
    .unwrap();
    let s = e
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
    (e, s)
}

#[test]
fn scheduled_long_running_executions_survive_restarts() {
    on_both("engine", |loc| {
        let c = orbit_compiler::compile_str("p.orbit", SRC);
        assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
        let clock = Arc::new(AtomicU64::new(
            orbit_runtime::schedule::parse_time("2026-11-15T00:00:00Z").unwrap(),
        ));
        let (mut e, s) = open(loc, &clock);
        e.register_definition(c.workflows[0].clone()).unwrap();
        e.create_schedule(CreateSchedule {
            schedule_id: "poll".into(),
            paused: false,
            spec: json!({ "workflow": "Poller", "cron": "*/5 * * * *" }),
        })
        .unwrap();
        clock.fetch_add(300_000, Ordering::SeqCst);
        e.tick().unwrap();
        let id = "poll:2026-11-15T00:05:00Z";
        let t = e.poll(&s).unwrap().expect("run 1 task");
        e.complete(&t.token, json!({ "next": 1, "done": false }))
            .unwrap();
        assert_eq!(e.execution(id).unwrap().run, 2);
        // Crash and reopen: run 2 is recovered and finishes.
        drop(e);
        let (mut e, s) = open(loc, &clock);
        clock.fetch_add(1_000, Ordering::SeqCst);
        e.tick().unwrap();
        let t = e.poll(&s).unwrap().expect("run 2 task");
        assert_eq!(t.input, json!({ "cursor": 1 }));
        assert_eq!(t.idempotency_key, format!("{id}/run2/fetch"));
        e.complete(&t.token, json!({ "next": 2, "done": true }))
            .unwrap();
        let st = e.execution(id).unwrap();
        assert_eq!((st.run, st.status), (2, WorkflowStatus::Completed));
        assert_eq!(st.schedule_id.as_deref(), Some("poll"));
        assert_eq!(e.run_history(id, 1).unwrap().len(), 6);
        let sched = e.get_schedule("poll").unwrap();
        assert_eq!(sched["fired"], 1);
        assert_eq!(sched["last_execution_id"], id);
        assert_eq!(sched["next_fire_at"], "2026-11-15T00:10:00Z");
    });
}
