//! Several active servers on one PostgreSQL database
//! (`orbit server --cluster --cluster-mode shared`): tasks claimed on one
//! node for executions another node owns, forwarded commands, ownership
//! fencing, takeover after a node stops heartbeating, and a single winner
//! when nodes take over concurrently.
//!
//! Runs only when `ORBIT_TEST_POSTGRES_URL` points at a server where the user
//! may create databases (see `tests/postgres.rs`). Each test creates (and
//! drops) its own database.

use orbit_ir::WorkflowIr;
use orbit_runtime::engine::{system_clock, ContractAnnouncement, RegisterWorker};
use orbit_runtime::identity::Identities;
use orbit_runtime::server::{serve_node, ServerConfig};
use orbit_runtime::shared::{self, Ctx, SharedConfig};
use orbit_runtime::{Engine, EngineConfig};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SRC: &str = r#"
task "a" { version "1.0.0" input { n: Int } output { n: Int } }
workflow One {
    version "1.0.0"
    input { n: Int }
    output { n: Int }
    step s { worker "a" with { n: input.n } }
    return { n: s.n }
}
workflow Waits {
    version "1.0.0"
    input { n: Int }
    signal go { by: String }
    step wait { await go timeout 1h }
    step s { worker "a" after wait with { n: input.n } }
}
workflow Loop {
    version "1.0.0"
    input { n: Int }
    output { n: Int }
    step s { worker "a" with { n: input.n } }
    continue_as_new when s.n < 1000 with { n: s.n }
    return { n: s.n }
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

    fn sql(&self) -> postgres::Client {
        let base = self.admin.rsplit_once('/').map(|(b, _)| b).unwrap();
        postgres::Client::connect(&format!("{base}/{}", self.name), postgres::NoTls).unwrap()
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

macro_rules! db_or_skip {
    ($name:literal) => {
        match Db::create($name) {
            Some(db) => db,
            None => {
                eprintln!("skipping: set ORBIT_TEST_POSTGRES_URL to run shared-mode tests");
                return;
            }
        }
    };
}

fn workflows() -> Vec<WorkflowIr> {
    let c = orbit_compiler::compile_str("shared.orbit", SRC);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    c.workflows.clone()
}

fn engine_config() -> EngineConfig {
    EngineConfig {
        session_timeout_ms: 1_000,
        heartbeat_interval_ms: 500,
        http_adapter: false,
        ..EngineConfig::default()
    }
}

fn shared_config(db: &str, id: &str) -> SharedConfig {
    let mut c = SharedConfig::new(db, id);
    c.heartbeat_interval = Duration::from_millis(200);
    c.node_timeout = Duration::from_millis(1_500);
    c
}

fn open(db: &str, id: &str) -> Engine {
    shared::open(shared_config(db, id), |store| {
        Engine::open(store, engine_config(), system_clock())
    })
    .unwrap()
}

fn worker_body() -> Value {
    json!({ "worker_id": "w", "runtime": "rust", "contracts": [{ "task_name": "a", "contract_version": "1.0.0" }] })
}

// ------------------------------------------------------------- HTTP nodes

struct TestNode {
    ctx: Arc<Ctx>,
    url: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestNode {
    fn spawn(db: &str, id: &str) -> TestNode {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let mut config = shared_config(db, id);
        config.advertise_addr = Some(url.clone());
        let engine = shared::open(config, |store| {
            Engine::open(store, engine_config(), system_clock())
        })
        .unwrap();
        let ctx = engine.shared_context().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move {
                    let cfg = ServerConfig {
                        addr,
                        auth_token: None,
                        worker_token: None,
                        identities: Identities::default(),
                        tls: None,
                        grpc_addr: None,
                        listener: Some(listener),
                        grpc_listener: None,
                        body_limit: 1 << 20,
                        tick_interval: Duration::from_millis(20),
                        ui: false,
                    };
                    serve_node(engine, cfg, None, async {
                        let _ = rx.await;
                    })
                    .await
                    .unwrap();
                });
        });
        let node = TestNode {
            ctx,
            url,
            stop: Some(tx),
            thread: Some(thread),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !node.ctx.is_ready() {
            assert!(Instant::now() < deadline, "{id} did not become ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        node
    }

    /// Stop serving. After `halt` this is a crash: the node never released
    /// its executions and stopped heartbeating.
    fn kill(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }

    fn crash(&mut self) {
        self.ctx.halt();
        self.kill();
    }

    fn health(&self) -> Value {
        ok("GET", &format!("{}/v1/health", self.url), None)
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        self.kill();
        std::thread::sleep(Duration::from_millis(300));
    }
}

fn call(method: &str, url: &str, body: Option<Value>) -> (u16, Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let mut resp = match (method, body) {
        ("GET", _) => agent.get(url).call(),
        (_, Some(b)) => agent.post(url).send_json(b),
        (_, None) => agent.post(url).send_empty(),
    }
    .unwrap();
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

fn ok(method: &str, url: &str, body: Option<Value>) -> Value {
    let (status, v) = call(method, url, body);
    assert!(status < 300, "{method} {url}: {status} {v}");
    v
}

fn register(node: &TestNode) -> String {
    ok(
        "POST",
        &format!("{}/v1/workers/register", node.url),
        Some(worker_body()),
    )["worker_session_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn poll(node: &TestNode, session: &str, wait_ms: u64) -> Option<Value> {
    let (s, v) = call(
        "POST",
        &format!("{}/v1/workers/poll", node.url),
        Some(json!({ "worker_session_id": session, "wait_ms": wait_ms })),
    );
    assert!(s == 200 || s == 204, "poll: {s} {v}");
    (s == 200).then_some(v)
}

fn complete(node: &TestNode, token: &Value, n: i64) -> Value {
    ok(
        "POST",
        &format!("{}/v1/tasks/complete", node.url),
        Some(json!({ "token": token, "output": { "n": n } })),
    )
}

fn start(node: &TestNode, workflow: &str, id: &str, n: i64) {
    ok(
        "POST",
        &format!("{}/v1/executions", node.url),
        Some(json!({ "workflow": workflow, "execution_id": id, "input": { "n": n } })),
    );
}

fn wait(node: &TestNode, id: &str) -> Value {
    ok(
        "GET",
        &format!("{}/v1/executions/{id}/wait?timeout_ms=15000", node.url),
        None,
    )
}

fn history_types(node: &TestNode, id: &str) -> Vec<String> {
    ok(
        "GET",
        &format!("{}/v1/executions/{id}/history", node.url),
        None,
    )["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

/// Work every task the session can get until `ids` all completed.
fn drain(node: &TestNode, session: &str, ids: &[String]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(t) = poll(node, session, 300) {
            let n = t["input"]["n"].as_i64().unwrap();
            complete(node, &t["token"], n * 10);
        }
        let done = ids.iter().all(|id| {
            ok("GET", &format!("{}/v1/executions/{id}", node.url), None)["status"] == "COMPLETED"
        });
        if done {
            return;
        }
        assert!(Instant::now() < deadline, "executions did not complete");
    }
}

fn metric(node: &TestNode, name: &str) -> u64 {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let text = agent
        .get(format!("{}/v1/metrics", node.url))
        .call()
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap();
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{name} ")))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

// ------------------------------------------------------------------- tests

#[test]
fn each_schedule_occurrence_is_fired_by_exactly_one_node() {
    let db = db_or_skip!("shared_schedules");
    let nodes = [
        TestNode::spawn(&db.url, "node-a"),
        TestNode::spawn(&db.url, "node-b"),
        TestNode::spawn(&db.url, "node-c"),
    ];
    for ir in workflows() {
        ok(
            "POST",
            &format!("{}/v1/workflows", nodes[0].url),
            Some(serde_json::to_value(ir).unwrap()),
        );
    }
    ok(
        "POST",
        &format!("{}/v1/schedules", nodes[1].url),
        Some(json!({
            "schedule_id": "tick",
            "workflow": "One",
            "interval": "1s",
            "overlap": "allow",
            "input": { "n": 1 }
        })),
    );
    std::thread::sleep(Duration::from_millis(5_000));
    ok(
        "POST",
        &format!("{}/v1/schedules/tick/pause", nodes[2].url),
        None,
    );
    // A claim in progress when pausing finishes within the tick.
    std::thread::sleep(Duration::from_millis(500));
    let s = ok("GET", &format!("{}/v1/schedules/tick", nodes[0].url), None);
    let mut ids: Vec<String> = ok(
        "GET",
        &format!("{}/v1/executions?limit=1000", nodes[0].url),
        None,
    )["executions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["execution_id"].as_str().map(str::to_string))
        .filter(|id| id.starts_with("tick:"))
        .collect();
    ids.sort();
    let fired = s["fired"].as_u64().unwrap();
    assert!(fired >= 3, "{s}");
    // Each start was claimed once: the schedule's count, the executions and
    // the per-node counters agree.
    assert_eq!(fired as usize, ids.len(), "{s} {ids:?}");
    let per_node: Vec<u64> = nodes
        .iter()
        .map(|n| metric(n, "orbit_schedules_fired_total"))
        .collect();
    assert_eq!(per_node.iter().sum::<u64>(), fired, "{per_node:?}");
    // Consecutive occurrences: none was handled twice or lost (skipped
    // ones are counted, not started). Skips can also precede the first
    // execution (a 1s occurrence ticked more than a second late), so the
    // span between the first and last execution is a lower bound.
    let first = orbit_runtime::schedule::parse_time(ids[0].trim_start_matches("tick:")).unwrap();
    let last = orbit_runtime::schedule::parse_time(ids.last().unwrap().trim_start_matches("tick:"))
        .unwrap();
    let span = (last - first) / 1000 + 1;
    assert!(fired <= span, "{s} {ids:?}");
    assert!(
        fired + s["skipped"].as_u64().unwrap() >= span,
        "{s} {ids:?}"
    );
}

#[test]
fn continue_as_new_keeps_the_owner_and_works_with_remote_workers() {
    let db = db_or_skip!("shared_can");
    let a = TestNode::spawn(&db.url, "node-a");
    let b = TestNode::spawn(&db.url, "node-b");
    for ir in workflows() {
        ok(
            "POST",
            &format!("{}/v1/workflows", a.url),
            Some(serde_json::to_value(ir).unwrap()),
        );
    }
    start(&a, "Loop", "loop-1", 1);
    // The worker polls node b; node a owns the execution through every run.
    let session = register(&b);
    drain(&b, &session, &["loop-1".to_string()]);
    let s = wait(&b, "loop-1");
    assert_eq!(s["status"], "COMPLETED");
    assert_eq!(s["run"], 3);
    assert_eq!(s["output"], json!({ "n": 1000 }));
    let run1 = ok(
        "GET",
        &format!("{}/v1/executions/loop-1/history?run=1", b.url),
        None,
    );
    assert_eq!(run1["run"], 1);
    assert_eq!(
        run1["events"].as_array().unwrap().last().unwrap()["type"],
        "WorkflowContinuedAsNew"
    );
    let mut c = db.sql();
    let tasks: i64 = c
        .query_one(
            "SELECT COUNT(*) FROM orbit_tasks WHERE execution_id = 'loop-1'",
            &[],
        )
        .unwrap()
        .get(0);
    // Only the last run's queue row is left (finished runs' rows are deleted).
    assert_eq!(tasks, 1);
    assert_eq!(a.health()["owned_executions"], 0);
}

#[test]
fn a_worker_on_one_node_runs_tasks_of_executions_another_node_owns() {
    let db = db_or_skip!("shared_flow");
    let a = TestNode::spawn(&db.url, "node-a");
    let b = TestNode::spawn(&db.url, "node-b");
    for ir in workflows() {
        ok(
            "POST",
            &format!("{}/v1/workflows", a.url),
            Some(serde_json::to_value(ir).unwrap()),
        );
    }
    let ids: Vec<String> = (0..5).map(|i| format!("e-{i}")).collect();
    for (i, id) in ids.iter().enumerate() {
        start(&a, "One", id, i as i64);
    }
    assert_eq!(a.health()["owned_executions"], 5);
    assert_eq!(b.health()["owned_executions"], 0);

    // The worker talks to node B only; node A owns the executions.
    let session = register(&b);
    let t = poll(&b, &session, 5_000).expect("a task claimed on node B");
    assert!(t["token"]
        .as_str()
        .unwrap()
        .starts_with(t["execution_id"].as_str().unwrap()));
    // The same session works through node A (sessions are cluster-wide).
    let t2 = poll(&a, &session, 5_000).expect("a task claimed on node A");
    assert_ne!(t["token"], t2["token"]);
    // A result reported to the node that did not hand the task out.
    assert_eq!(complete(&a, &t["token"], 1)["accepted"], true);
    assert_eq!(complete(&b, &t2["token"], 2)["accepted"], true);
    drain(&b, &session, &ids);
    for id in &ids {
        let s = wait(&b, id);
        assert_eq!(s["status"], "COMPLETED", "{s}");
        let types = history_types(&b, id);
        assert_eq!(
            types.iter().filter(|t| *t == "StepDispatched").count(),
            1,
            "{types:?}"
        );
    }
    assert!(b.health()["tasks_claimed"].as_u64().unwrap() >= 4);
    assert_eq!(
        a.health()["owned_executions"],
        0,
        "finished executions are released"
    );
    // Both nodes see the worker online, wherever it registered.
    let workers = ok("GET", &format!("{}/v1/workers", a.url), None);
    assert_eq!(workers["workers"][0]["online"], true, "{workers}");
    // A duplicate result is refused like on one server.
    let dup = complete(&b, &t["token"], 1);
    assert_eq!(dup["accepted"], false, "{dup}");

    // Signals and cancellations sent to a node that does not own the
    // execution are applied by the owner and answered as on one server.
    start(&a, "Waits", "w-1", 3);
    start(&a, "Waits", "w-2", 4);
    let (s, v) = call(
        "POST",
        &format!("{}/v1/executions/w-1/signals/go", b.url),
        Some(json!({ "by": 1 })),
    );
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (422, Some("SIGNAL_PAYLOAD_INVALID"))
    );
    let (s, v) = call(
        "POST",
        &format!("{}/v1/executions/w-1/signals/nope", b.url),
        Some(json!({})),
    );
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (404, Some("UNKNOWN_SIGNAL"))
    );
    let sent = ok(
        "POST",
        &format!("{}/v1/executions/w-1/signals/go", b.url),
        Some(json!({ "by": "ops" })),
    );
    let hist = ok("GET", &format!("{}/v1/executions/w-1/history", b.url), None);
    let received = hist["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "SignalReceived")
        .expect("signal recorded")
        .clone();
    assert_eq!(sent["seq"], received["seq"], "{sent}");
    let cancelled = ok("POST", &format!("{}/v1/executions/w-2/cancel", b.url), None);
    assert_eq!(cancelled["status"], "CANCELLED", "{cancelled}");
    let (s, v) = call("POST", &format!("{}/v1/executions/w-2/cancel", b.url), None);
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (409, Some("EXECUTION_FINISHED"))
    );
    let t = poll(&b, &session, 5_000).expect("the signalled execution continues");
    assert_eq!(t["execution_id"], "w-1");
    complete(&b, &t["token"], 30);
    assert_eq!(wait(&b, "w-1")["status"], "COMPLETED");

    // Readiness on every live node.
    for n in [&a, &b] {
        let (s, v) = call("GET", &format!("{}/v1/ready", n.url), None);
        assert_eq!((s, v["role"].as_str()), (200, Some("shared")), "{v}");
        assert_eq!(v["nodes"].as_array().unwrap().len(), 2, "{v}");
    }
}

#[test]
fn a_stale_owner_cannot_append() {
    let db = db_or_skip!("shared_fencing");
    let mut old = open(&db.url, "node-old");
    for ir in workflows() {
        old.register_definition(ir).unwrap();
    }
    let session = old
        .register_worker(RegisterWorker {
            worker_id: "w".into(),
            runtime: "rust".into(),
            sdk: None,
            contracts: vec![ContractAnnouncement {
                task_name: "a".into(),
                contract_version: "1.0.0".into(),
            }],
        })
        .unwrap()
        .worker_session_id;
    old.start("One", None, json!({ "n": 1 }), Some("f-1".into()))
        .unwrap();
    let task = old.poll(&session).unwrap().expect("a task");

    // Another server takes the execution over: the epoch moves on.
    let mut sql = db.sql();
    let epoch: i64 = sql
        .query_one(
            "UPDATE orbit_owners SET node_id = 'node-new', epoch = epoch + 1 WHERE execution_id = 'f-1' RETURNING epoch",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(epoch, 2);
    let err = old.complete(&task.token, json!({ "n": 2 })).unwrap_err();
    assert_eq!(err.code, "STORAGE_ERROR");
    assert!(err.message.contains("fenced"), "{}", err.message);
    let history: i64 = sql
        .query_one(
            "SELECT count(*) FROM execution_events WHERE execution_id = 'f-1' AND event_type = 'StepSucceeded'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(history, 0, "the fenced append left no trace");

    // `node-new` is not running: a live node takes the execution over from
    // it, and accepts the result the old owner could not record.
    let b = TestNode::spawn(&db.url, "node-b");
    let deadline = Instant::now() + Duration::from_secs(10);
    while b.health()["owned_executions"] != 1 {
        assert!(Instant::now() < deadline, "node-b did not take over");
        std::thread::sleep(Duration::from_millis(50));
    }
    let owner: (String, i64) = sql
        .query_one(
            "SELECT node_id, epoch FROM orbit_owners WHERE execution_id = 'f-1'",
            &[],
        )
        .map(|r| (r.get(0), r.get(1)))
        .unwrap();
    assert_eq!(owner, ("node-b".to_string(), 3));
    assert_eq!(complete(&b, &json!(task.token), 2)["accepted"], true);
    assert_eq!(wait(&b, "f-1")["status"], "COMPLETED");
    assert_eq!(wait(&b, "f-1")["output"]["n"], 2);
}

#[test]
fn live_nodes_take_over_from_a_node_that_stops_heartbeating() {
    let db = db_or_skip!("shared_takeover");
    let mut a = TestNode::spawn(&db.url, "node-a");
    let b = TestNode::spawn(&db.url, "node-b");
    let c = TestNode::spawn(&db.url, "node-c");
    for ir in workflows() {
        ok(
            "POST",
            &format!("{}/v1/workflows", a.url),
            Some(serde_json::to_value(ir).unwrap()),
        );
    }
    let ids: Vec<String> = (0..20).map(|i| format!("t-{i}")).collect();
    for (i, id) in ids.iter().enumerate() {
        start(&a, "One", id, i as i64);
    }
    assert_eq!(a.health()["owned_executions"], 20);
    // One attempt is in flight on a worker that talks to node B.
    let session = register(&b);
    let held = poll(&b, &session, 5_000).expect("a task");

    let crashed = Instant::now();
    a.crash();
    let deadline = Instant::now() + Duration::from_secs(15);
    let (owned_b, owned_c) = loop {
        let (ob, oc) = (
            b.health()["owned_executions"].as_u64().unwrap(),
            c.health()["owned_executions"].as_u64().unwrap(),
        );
        if ob + oc == 20 {
            break (ob, oc);
        }
        assert!(
            Instant::now() < deadline,
            "takeover incomplete: {ob} + {oc}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let took = crashed.elapsed();
    assert!(took < Duration::from_secs(6), "takeover took {took:?}");
    assert!(
        owned_b > 0 && owned_c > 0,
        "takeovers are spread: {owned_b} / {owned_c}"
    );
    let mut sql = db.sql();
    let rows: Vec<(String, i64)> = sql
        .query("SELECT node_id, epoch FROM orbit_owners", &[])
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(rows.len(), 20);
    assert!(
        rows.iter().all(|(n, e)| n != "node-a" && *e == 2),
        "{rows:?}"
    );

    // The attempt in flight survives the owner's death: its result goes to
    // the new owner through any node.
    assert_eq!(complete(&c, &held["token"], 7)["accepted"], true);
    drain(&c, &session, &ids);
    let id = held["execution_id"].as_str().unwrap();
    assert_eq!(wait(&b, id)["output"]["n"], 7);
    let types = history_types(&b, id);
    assert!(
        !types.contains(&"StepAttemptUncertain".to_string()),
        "{types:?}"
    );
}

#[test]
fn exactly_one_node_wins_each_execution_under_concurrent_takeover() {
    let db = db_or_skip!("shared_race");
    // Creates the shared tables.
    let _engine = open(&db.url, "setup");
    let mut sql = db.sql();
    for root in 0..40 {
        for child in 0..3 {
            let id = if child == 0 {
                format!("r{root}")
            } else {
                format!("r{root}.c{child}")
            };
            sql.execute(
                "INSERT INTO orbit_owners (execution_id, root_id, node_id, epoch, since_ms) VALUES ($1, $2, 'dead', 1, 0)",
                &[&id, &format!("r{root}")],
            )
            .unwrap();
        }
    }
    let url = db.url.clone();
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let url = url.clone();
            std::thread::spawn(move || {
                let mut c = postgres::Client::connect(&url, postgres::NoTls).unwrap();
                let me = format!("node-{t}");
                let mut won = vec![];
                // Every thread tries every tree, in a different order.
                let mut roots: Vec<usize> = (0..40).collect();
                roots.rotate_left(t * 5);
                for r in roots {
                    for (id, epoch) in
                        shared::take_over_root(&mut c, &me, &format!("r{r}"), "dead").unwrap()
                    {
                        won.push((id, epoch, me.clone()));
                    }
                }
                won
            })
        })
        .collect();
    let mut winners: HashMap<String, Vec<String>> = HashMap::new();
    for t in threads {
        for (id, epoch, me) in t.join().unwrap() {
            assert_eq!(epoch, 2);
            winners.entry(id).or_default().push(me);
        }
    }
    assert_eq!(winners.len(), 120);
    for (id, w) in &winners {
        assert_eq!(w.len(), 1, "{id} won by {w:?}");
        let owner: String = sql
            .query_one(
                "SELECT node_id FROM orbit_owners WHERE execution_id = $1",
                &[id],
            )
            .unwrap()
            .get(0);
        assert_eq!(&owner, &w[0]);
    }
    // A tree moves as a whole.
    for root in 0..40 {
        let owners: Vec<String> = sql
            .query(
                "SELECT DISTINCT node_id FROM orbit_owners WHERE root_id = $1",
                &[&format!("r{root}")],
            )
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(owners.len(), 1, "{owners:?}");
    }
}
