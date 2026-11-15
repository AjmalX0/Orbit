//! Schedules (`/v1/schedules`) driven by a manual clock: occurrences, ids,
//! overlap and catch-up policies, pausing, triggers and crash recovery.

use orbit_ir::WorkflowIr;
use orbit_runtime::engine::{ContractAnnouncement, RegisterWorker, TaskAssignment};
use orbit_runtime::schedule::{parse_time, CreateSchedule, CLAIM_MS};
use orbit_runtime::{Engine, EngineConfig, Store};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const SRC: &str = r#"
task "job" { version "1.0.0" input { n: Int } output { n: Int } }

workflow Job {
    version "1.0.0"
    input { n: Int = 1 }
    output { n: Int }
    step run { worker "job" with { n: input.n } }
    return { n: run.n }
}
"#;

/// 2026-11-15T00:00:00Z
const T0: &str = "2026-11-15T00:00:00Z";
const MIN: u64 = 60_000;

fn workflow() -> WorkflowIr {
    let c = orbit_compiler::compile_str("job.orbit", SRC);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    c.workflows.into_iter().next().unwrap()
}

struct Harness {
    engine: Engine,
    clock: Arc<AtomicU64>,
    db: PathBuf,
    session: String,
}

impl Harness {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("orbit-sched-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let db = dir.join("orbit.db");
        let _ = std::fs::remove_file(&db);
        let clock = Arc::new(AtomicU64::new(parse_time(T0).unwrap()));
        let mut h = Self::open(db, clock);
        h.engine.register_definition(workflow()).unwrap();
        h
    }

    fn open(db: PathBuf, clock: Arc<AtomicU64>) -> Self {
        let c = clock.clone();
        let mut engine = Engine::open(
            Store::open(&db).unwrap(),
            EngineConfig {
                // The test worker polls rarely; it must not be declared lost.
                session_timeout_ms: 1 << 40,
                ..EngineConfig::default()
            },
            Box::new(move || c.load(Ordering::SeqCst)),
        )
        .unwrap();
        let session = engine
            .register_worker(RegisterWorker {
                worker_id: "w".into(),
                runtime: "python".into(),
                sdk: None,
                contracts: vec![ContractAnnouncement {
                    task_name: "job".into(),
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

    /// Crash: drop the engine and reopen the database.
    fn restart(self) -> Self {
        let (db, clock) = (self.db.clone(), self.clock.clone());
        drop(self.engine);
        Self::open(db, clock)
    }

    fn now(&self) -> u64 {
        self.clock.load(Ordering::SeqCst)
    }

    fn advance(&mut self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
        self.engine.tick().unwrap();
    }

    fn create(&mut self, id: &str, spec: Value) -> Value {
        self.engine
            .create_schedule(CreateSchedule {
                schedule_id: id.into(),
                paused: false,
                spec,
            })
            .unwrap()
    }

    fn poll(&mut self) -> Option<TaskAssignment> {
        self.engine.poll(&self.session).unwrap()
    }

    /// Complete every queued task; returns the execution ids served.
    fn work(&mut self) -> Vec<String> {
        let mut out = vec![];
        while let Some(t) = self.poll() {
            self.engine.complete(&t.token, json!({ "n": 7 })).unwrap();
            out.push(t.execution_id);
        }
        out
    }

    fn executions(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .engine
            .list_executions(None, None, 100)
            .unwrap()
            .into_iter()
            .map(|e| (e.execution_id, e.status))
            .collect();
        v.sort();
        v
    }

    fn schedule(&self, id: &str) -> Value {
        self.engine.get_schedule(id).unwrap()
    }
}

#[test]
fn an_interval_schedule_starts_one_execution_per_occurrence() {
    let mut h = Harness::new("interval");
    let s = h.create(
        "every5",
        json!({ "workflow": "Job", "interval": "5m", "input": { "n": 3 } }),
    );
    assert_eq!(s["next_fire_at"], "2026-11-15T00:05:00Z");
    assert_eq!(
        s["next_fire_times"],
        json!([
            "2026-11-15T00:05:00Z",
            "2026-11-15T00:10:00Z",
            "2026-11-15T00:15:00Z",
            "2026-11-15T00:20:00Z",
            "2026-11-15T00:25:00Z"
        ])
    );
    h.advance(4 * MIN);
    assert!(h.executions().is_empty());
    h.advance(MIN);
    let id = "every5:2026-11-15T00:05:00Z";
    assert_eq!(h.executions(), [(id.to_string(), "RUNNING".to_string())]);
    let state = h.engine.execution(id).unwrap();
    assert_eq!(state.schedule_id.as_deref(), Some("every5"));
    assert_eq!(state.input, json!({ "n": 3 }));
    assert_eq!(h.work(), [id]);
    // Ticking again within the same occurrence starts nothing new.
    h.advance(1_000);
    h.advance(1_000);
    assert_eq!(h.executions().len(), 1);
    h.advance(5 * MIN);
    h.work();
    let s = h.schedule("every5");
    assert_eq!(s["fired"], 2);
    assert_eq!(s["skipped"], 0);
    assert_eq!(s["last_execution_id"], "every5:2026-11-15T00:10:00Z");
    assert_eq!(s["last_fire_at"], "2026-11-15T00:10:00Z");
    assert_eq!(s["next_fire_at"], "2026-11-15T00:15:00Z");
    assert_eq!(h.engine.metrics.schedules_fired, 2);
    assert!(h
        .executions()
        .iter()
        .all(|(_, status)| status == "COMPLETED"));
}

#[test]
fn a_cron_schedule_fires_in_its_time_zone() {
    let mut h = Harness::new("cron-tz");
    let s = h.create(
        "nightly",
        json!({ "workflow": "Job", "cron": "30 2 * * *", "timezone": "Europe/Berlin" }),
    );
    // 02:30 in Berlin is 01:30 UTC in November.
    assert_eq!(s["next_fire_at"], "2026-11-15T01:30:00Z");
    h.advance(90 * MIN);
    assert_eq!(
        h.executions(),
        [(
            "nightly:2026-11-15T01:30:00Z".to_string(),
            "RUNNING".to_string()
        )]
    );
    assert_eq!(
        h.schedule("nightly")["next_fire_at"],
        "2026-11-16T01:30:00Z"
    );
}

#[test]
fn a_fire_is_never_started_twice() {
    let mut h = Harness::new("idempotent");
    h.create("s", json!({ "workflow": "Job", "interval": "1m" }));
    // A client (or a crashed earlier attempt) already started the id.
    h.engine
        .start(
            "Job",
            None,
            json!({}),
            Some("s:2026-11-15T00:01:00Z".to_string()),
        )
        .unwrap();
    h.advance(MIN);
    assert_eq!(h.executions().len(), 1);
    let s = h.schedule("s");
    assert_eq!(s["last_execution_id"], "s:2026-11-15T00:01:00Z");
    assert_eq!(s["next_fire_at"], "2026-11-15T00:02:00Z");
    // `created: false`: not counted as a new start.
    assert_eq!(h.engine.metrics.schedules_fired, 0);
}

#[test]
fn overlap_skip_does_not_start_while_the_previous_run_is_running() {
    let mut h = Harness::new("overlap-skip");
    h.create("s", json!({ "workflow": "Job", "interval": "1m" }));
    h.advance(MIN);
    h.advance(MIN);
    // The first execution has not finished: the second occurrence is skipped.
    assert_eq!(h.executions().len(), 1);
    let s = h.schedule("s");
    assert_eq!(s["skipped"], 1);
    assert_eq!(s["fired"], 1);
    assert_eq!(h.engine.metrics.schedules_skipped_overlap, 1);
    h.work();
    h.advance(MIN);
    assert_eq!(h.executions().len(), 2);
    assert!(h
        .executions()
        .iter()
        .any(|(id, _)| id == "s:2026-11-15T00:03:00Z"));
}

#[test]
fn overlap_allow_and_cancel_previous() {
    let mut h = Harness::new("overlap-allow");
    h.create(
        "a",
        json!({ "workflow": "Job", "interval": "1m", "overlap": "allow" }),
    );
    h.advance(MIN);
    h.advance(MIN);
    assert_eq!(
        h.executions()
            .iter()
            .filter(|(_, s)| s == "RUNNING")
            .count(),
        2
    );

    let mut h = Harness::new("overlap-cancel");
    h.create(
        "c",
        json!({ "workflow": "Job", "interval": "1m", "overlap": "cancel_previous" }),
    );
    h.advance(MIN);
    h.advance(MIN);
    assert_eq!(
        h.executions(),
        [
            (
                "c:2026-11-15T00:01:00Z".to_string(),
                "CANCELLED".to_string()
            ),
            ("c:2026-11-15T00:02:00Z".to_string(), "RUNNING".to_string())
        ]
    );
}

#[test]
fn missed_occurrences_are_skipped_after_downtime() {
    let mut h = Harness::new("catchup-skip");
    h.create("s", json!({ "workflow": "Job", "interval": "10m" }));
    let h = h.restart();
    // The server was down from 00:00 to 00:35: 00:10, 00:20 and 00:30 passed.
    h.clock.fetch_add(35 * MIN, Ordering::SeqCst);
    let mut h = h.restart();
    h.advance(0);
    assert!(h.executions().is_empty());
    let s = h.schedule("s");
    assert_eq!(s["skipped"], 3);
    assert_eq!(s["next_fire_at"], "2026-11-15T00:40:00Z");
    assert_eq!(h.engine.metrics.schedules_skipped_missed, 3);
    h.advance(5 * MIN);
    assert_eq!(h.executions().len(), 1);
}

#[test]
fn run_once_starts_the_latest_missed_occurrence() {
    let mut h = Harness::new("catchup-once");
    h.create(
        "s",
        json!({ "workflow": "Job", "interval": "10m", "catchup": "run_once" }),
    );
    h.clock.fetch_add(35 * MIN, Ordering::SeqCst);
    let mut h = h.restart();
    h.advance(0);
    assert_eq!(
        h.executions(),
        [("s:2026-11-15T00:30:00Z".to_string(), "RUNNING".to_string())]
    );
    let s = h.schedule("s");
    assert_eq!(s["skipped"], 2);
    assert_eq!(s["fired"], 1);
    assert_eq!(s["next_fire_at"], "2026-11-15T00:40:00Z");
}

#[test]
fn an_occurrence_claimed_by_a_crashed_server_is_fired_after_the_claim_expires() {
    let mut h = Harness::new("claim-crash");
    h.create("s", json!({ "workflow": "Job", "interval": "1m" }));
    let occurrence = parse_time("2026-11-15T00:01:00Z").unwrap();
    // Another server claimed the occurrence and died before starting it.
    let other = Store::open(&h.db).unwrap();
    let now = h.now() + MIN;
    assert!(other
        .claim_schedule("s", occurrence, "dead-server", now, now + CLAIM_MS)
        .unwrap());
    // Nobody else may take it while the claim lasts.
    assert!(!other
        .claim_schedule("s", occurrence, "third", now, now + CLAIM_MS)
        .unwrap());
    h.advance(MIN);
    assert!(h.executions().is_empty());
    h.advance(CLAIM_MS);
    // The claim expired: the occurrence fires (late, within the catch-up grace).
    assert_eq!(
        h.executions(),
        [("s:2026-11-15T00:01:00Z".to_string(), "RUNNING".to_string())]
    );
    assert_eq!(h.schedule("s")["next_fire_at"], "2026-11-15T00:02:00Z");
}

#[test]
fn a_restarted_single_server_drops_claims_left_by_its_previous_process() {
    let mut h = Harness::new("claim-restart");
    h.create("s", json!({ "workflow": "Job", "interval": "1m" }));
    let occurrence = parse_time("2026-11-15T00:01:00Z").unwrap();
    let other = Store::open(&h.db).unwrap();
    let now = h.now() + MIN;
    assert!(other
        .claim_schedule("s", occurrence, "previous-process", now, now + CLAIM_MS)
        .unwrap());
    drop(other);
    let mut h = h.restart();
    h.advance(MIN);
    assert_eq!(
        h.executions(),
        [("s:2026-11-15T00:01:00Z".to_string(), "RUNNING".to_string())]
    );
}

#[test]
fn schedules_survive_a_restart_and_fire_exactly_once() {
    let mut h = Harness::new("restart");
    h.create("s", json!({ "workflow": "Job", "interval": "1m" }));
    h.advance(MIN);
    let mut h = h.restart();
    h.advance(1_000);
    h.advance(1_000);
    assert_eq!(h.executions().len(), 1);
    h.work();
    h.advance(MIN);
    assert_eq!(h.executions().len(), 2);
    assert_eq!(h.schedule("s")["fired"], 2);
}

#[test]
fn pause_resume_trigger_update_and_delete() {
    let mut h = Harness::new("pause");
    h.create("s", json!({ "workflow": "Job", "interval": "1m" }));
    let paused = h.engine.set_schedule_paused("s", true).unwrap();
    assert_eq!(paused["paused"], true);
    assert_eq!(paused["next_fire_at"], Value::Null);
    h.advance(3 * MIN);
    assert!(h.executions().is_empty());
    // Resuming does not catch up on what was missed while paused.
    let resumed = h.engine.set_schedule_paused("s", false).unwrap();
    assert_eq!(resumed["next_fire_at"], "2026-11-15T00:04:00Z");
    // Trigger: an execution now, outside the timing.
    let t = h.engine.trigger_schedule("s").unwrap();
    assert!(t.started);
    assert_eq!(
        t.execution_id.as_deref(),
        Some("s:manual:2026-11-15T00:03:00Z")
    );
    // The overlap policy applies to triggers too.
    let t = h.engine.trigger_schedule("s").unwrap();
    assert!(!t.started);
    assert_eq!(t.skipped.as_deref(), Some("overlap"));
    h.work();
    // Update: switch to cron; the next occurrence is recomputed.
    let u = h
        .engine
        .update_schedule("s", json!({ "cron": "0 6 * * *", "interval": null }))
        .unwrap();
    assert_eq!(u["cron"], "0 6 * * *");
    assert!(u.get("interval").is_none());
    assert_eq!(u["next_fire_at"], "2026-11-15T06:00:00Z");
    assert_eq!(u["fired"], 1);
    h.engine.delete_schedule("s").unwrap();
    assert_eq!(
        h.engine.get_schedule("s").unwrap_err().code,
        "SCHEDULE_NOT_FOUND"
    );
    assert_eq!(
        h.engine.delete_schedule("s").unwrap_err().code,
        "SCHEDULE_NOT_FOUND"
    );
}

#[test]
fn start_and_end_times_bound_the_schedule() {
    let mut h = Harness::new("bounds");
    let s = h.create(
        "s",
        json!({
            "workflow": "Job",
            "interval": "1m",
            "start_at": "2026-11-15T00:02:30Z",
            "end_at": "2026-11-15T00:04:00Z",
            "overlap": "allow"
        }),
    );
    assert_eq!(
        s["next_fire_times"],
        json!(["2026-11-15T00:02:30Z", "2026-11-15T00:03:30Z"])
    );
    for _ in 0..6 {
        h.advance(MIN);
    }
    assert_eq!(h.executions().len(), 2);
    let s = h.schedule("s");
    assert_eq!(s["next_fire_at"], Value::Null);
    assert_eq!(s["next_fire_times"], json!([]));
}

#[test]
fn invalid_schedules_are_rejected() {
    let mut h = Harness::new("invalid");
    let err = |h: &mut Harness, id: &str, spec: Value| {
        h.engine
            .create_schedule(CreateSchedule {
                schedule_id: id.into(),
                paused: false,
                spec,
            })
            .unwrap_err()
    };
    let e = err(&mut h, "x", json!({ "workflow": "Nope", "interval": "1m" }));
    assert_eq!((e.status, e.code), (404, "WORKFLOW_NOT_FOUND"));
    let e = err(
        &mut h,
        "x",
        json!({ "workflow": "Job", "interval": "1m", "input": { "n": "one" } }),
    );
    assert_eq!(e.code, "INVALID_INPUT");
    let e = err(
        &mut h,
        "x",
        json!({ "workflow": "Job", "cron": "61 * * * *" }),
    );
    assert_eq!(e.code, "INVALID_SCHEDULE");
    assert!(e.message.contains("out of range"), "{}", e.message);
    let e = err(
        &mut h,
        "bad/id",
        json!({ "workflow": "Job", "interval": "1m" }),
    );
    assert_eq!(e.code, "INVALID_SCHEDULE");
    let e = err(
        &mut h,
        "x",
        json!({ "workflow": "Job", "interval": "1m", "colour": "red" }),
    );
    assert!(e.message.contains("unknown field"), "{}", e.message);
    h.create("x", json!({ "workflow": "Job", "interval": "1m" }));
    let e = err(&mut h, "x", json!({ "workflow": "Job", "interval": "1m" }));
    assert_eq!((e.status, e.code), (409, "SCHEDULE_EXISTS"));
}

#[test]
fn a_failing_start_is_retried_and_reported() {
    let mut h = Harness::new("retry");
    h.engine.config.limits.max_active_executions = Some(1);
    h.engine
        .start("Job", None, json!({}), Some("blocker".into()))
        .unwrap();
    h.create("s", json!({ "workflow": "Job", "interval": "1m" }));
    h.advance(MIN);
    assert_eq!(h.executions().len(), 1);
    let s = h.schedule("s");
    assert!(
        s["last_error"]
            .as_str()
            .unwrap()
            .contains("already running"),
        "{s}"
    );
    // Not advanced: once there is room, the same occurrence starts.
    assert_eq!(s["next_fire_at"], "2026-11-15T00:01:00Z");
    h.work();
    h.advance(10_000);
    assert!(h
        .executions()
        .iter()
        .any(|(id, _)| id == "s:2026-11-15T00:01:00Z"));
    assert_eq!(h.schedule("s")["last_error"], Value::Null);
}
