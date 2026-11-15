//! Several servers on one PostgreSQL database (`orbit server --cluster`):
//! leader election, fencing of a deposed leader, and failover recovery.
//!
//! Runs only when `ORBIT_TEST_POSTGRES_URL` points at a server where the user
//! may create databases (see `tests/postgres.rs`). Each test creates (and
//! drops) its own database, so advisory locks do not cross tests.

use orbit_ir::WorkflowIr;
use orbit_runtime::cluster::{ClusterConfig, Node};
use orbit_runtime::engine::{system_clock, ContractAnnouncement, RegisterWorker};
use orbit_runtime::identity::Identities;
use orbit_runtime::server::{serve_node, ServerConfig};
use orbit_runtime::store::{PgMode, PgStore};
use orbit_runtime::{Engine, EngineConfig, Store};
use serde_json::{json, Value};
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
                eprintln!("skipping: set ORBIT_TEST_POSTGRES_URL to run cluster tests");
                return;
            }
        }
    };
}

fn workflow() -> WorkflowIr {
    let c = orbit_compiler::compile_str("cluster.orbit", SRC);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    c.workflows[0].clone()
}

fn engine_config() -> EngineConfig {
    EngineConfig {
        session_timeout_ms: 1_000,
        heartbeat_interval_ms: 500,
        http_adapter: false,
        ..EngineConfig::default()
    }
}

fn worker_req() -> RegisterWorker {
    RegisterWorker {
        worker_id: "w".into(),
        runtime: "rust".into(),
        sdk: None,
        contracts: vec![ContractAnnouncement {
            task_name: "a".into(),
            contract_version: "1.0.0".into(),
        }],
    }
}

// ------------------------------------------------------------- HTTP nodes

struct TestNode {
    node: Arc<Node>,
    url: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestNode {
    fn spawn(db: &str, id: &str) -> TestNode {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let mut config = ClusterConfig::new(db, id);
        config.advertise_addr = Some(url.clone());
        let node = Node::new(
            config,
            Box::new(|store| Engine::open(store, engine_config(), system_clock())),
        );
        let standby = node.standby_engine().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let n = node.clone();
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
                    serve_node(standby, cfg, Some(n), async {
                        let _ = rx.await;
                    })
                    .await
                    .unwrap();
                });
        });
        TestNode {
            node,
            url,
            stop: Some(tx),
            thread: Some(thread),
        }
    }

    /// Stop serving and leave the election (like the process going away).
    fn kill(&mut self) {
        self.node.stop();
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }

    fn role(&self) -> Option<String> {
        call("GET", &format!("{}/v1/health", self.url), None)
            .ok()
            .and_then(|(_, v, _)| v["role"].as_str().map(str::to_string))
    }
}

impl Drop for TestNode {
    fn drop(&mut self) {
        self.kill();
        // Let the election thread notice the stop and release its session.
        std::thread::sleep(Duration::from_millis(1_200));
    }
}

type Reply = (u16, Value, Option<String>);

fn call(method: &str, url: &str, body: Option<Value>) -> Result<Reply, ureq::Error> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let mut resp = match (method, body) {
        ("GET", _) => agent.get(url).call()?,
        (_, Some(b)) => agent.post(url).send_json(b)?,
        (_, None) => agent.post(url).send_empty()?,
    };
    let leader = resp
        .headers()
        .get("x-orbit-leader")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    Ok((
        status,
        serde_json::from_str(&text).unwrap_or(Value::Null),
        leader,
    ))
}

fn ok(method: &str, url: &str, body: Option<Value>) -> Value {
    let (status, v, _) = call(method, url, body).unwrap();
    assert!(status < 300, "{method} {url}: {status} {v}");
    v
}

/// Wait until exactly one of `nodes` is the leader; return its index.
fn await_leader(nodes: &[&TestNode], within: Duration) -> usize {
    let deadline = Instant::now() + within;
    loop {
        let roles: Vec<Option<String>> = nodes.iter().map(|n| n.role()).collect();
        let leaders: Vec<usize> = (0..nodes.len())
            .filter(|i| roles[*i].as_deref() == Some("leader"))
            .collect();
        assert!(leaders.len() <= 1, "two leaders at once: {roles:?}");
        if leaders.len() == 1
            && roles
                .iter()
                .all(|r| matches!(r.as_deref(), Some("leader" | "standby")))
        {
            return leaders[0];
        }
        assert!(Instant::now() < deadline, "no leader elected: {roles:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ------------------------------------------------------------------- tests

#[test]
fn exactly_one_leader_and_standbys_redirect_writes() {
    let db = db_or_skip!("cluster_election");
    let a = TestNode::spawn(&db.url, "node-a");
    let b = TestNode::spawn(&db.url, "node-b");
    let nodes = [&a, &b];
    let l = await_leader(&nodes, Duration::from_secs(10));
    let (leader, standby) = (nodes[l], nodes[1 - l]);
    // Stable: still exactly one leader over several check intervals.
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        assert_eq!(await_leader(&nodes, Duration::from_secs(1)), l);
        std::thread::sleep(Duration::from_millis(100));
    }
    let health = ok("GET", &format!("{}/v1/health", standby.url), None);
    assert_eq!(health["role"], "standby");
    assert_eq!(health["leader"]["addr"], leader.url.as_str());
    assert_eq!(health["term"], leader.node.term());
    // Readiness for load balancers.
    let (s, v, _) = call("GET", &format!("{}/v1/ready", leader.url), None).unwrap();
    assert_eq!((s, v["ready"].clone()), (200, json!(true)));
    let (s, v, _) = call("GET", &format!("{}/v1/ready", standby.url), None).unwrap();
    assert_eq!((s, v["ready"].clone()), (503, json!(false)));
    // Writes and worker calls on the standby name the leader.
    let ir = serde_json::to_value(workflow()).unwrap();
    for (path, body) in [
        ("/v1/workflows", ir.clone()),
        (
            "/v1/executions",
            json!({ "workflow": "One", "input": { "n": 1 } }),
        ),
        (
            "/v1/workers/register",
            serde_json::to_value(json!({ "worker_id": "w", "runtime": "rust", "contracts": [] }))
                .unwrap(),
        ),
        ("/v1/tasks/complete", json!({ "token": "t" })),
    ] {
        let (s, v, header) = call("POST", &format!("{}{path}", standby.url), Some(body)).unwrap();
        assert_eq!(s, 503, "{path}: {v}");
        assert_eq!(v["error"]["code"], "NOT_LEADER");
        assert_eq!(v["error"]["leader_addr"], leader.url.as_str());
        assert_eq!(header.as_deref(), Some(leader.url.as_str()));
    }
    // Writes on the leader; reads on the standby come from the database.
    ok("POST", &format!("{}/v1/workflows", leader.url), Some(ir));
    let started = ok(
        "POST",
        &format!("{}/v1/executions", leader.url),
        Some(json!({ "workflow": "One", "input": { "n": 7 }, "execution_id": "e-1" })),
    );
    assert_eq!(started["execution_id"], "e-1");
    let wfs = ok("GET", &format!("{}/v1/workflows", standby.url), None);
    assert_eq!(wfs["workflows"][0]["name"], "One");
    let ex = ok("GET", &format!("{}/v1/executions/e-1", standby.url), None);
    assert_eq!(ex["status"], "RUNNING");
    let list = ok("GET", &format!("{}/v1/executions", standby.url), None);
    assert_eq!(list["executions"][0]["execution_id"], "e-1");
    let hist = ok(
        "GET",
        &format!("{}/v1/executions/e-1/history", standby.url),
        None,
    );
    assert_eq!(hist["events"][0]["type"], "WorkflowCreated");
}

fn metric(url: &str, name: &str) -> u64 {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let text = agent
        .get(format!("{url}/v1/metrics"))
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

fn scheduled_ids(url: &str, prefix: &str) -> Vec<String> {
    let mut ids: Vec<String> = ok("GET", &format!("{url}/v1/executions?limit=1000"), None)
        ["executions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["execution_id"].as_str().map(str::to_string))
        .filter(|id| id.starts_with(prefix))
        .collect();
    ids.sort();
    ids
}

#[test]
fn only_the_leader_fires_schedules_and_a_new_leader_carries_on() {
    let db = db_or_skip!("cluster_schedules");
    let mut a = TestNode::spawn(&db.url, "node-a");
    await_leader(&[&a], Duration::from_secs(10));
    let b = TestNode::spawn(&db.url, "node-b");
    assert_eq!(await_leader(&[&a, &b], Duration::from_secs(10)), 0);
    ok(
        "POST",
        &format!("{}/v1/workflows", a.url),
        Some(serde_json::to_value(workflow()).unwrap()),
    );
    let spec = json!({
        "schedule_id": "tick",
        "workflow": "One",
        "interval": "1s",
        "overlap": "allow",
        "input": { "n": 1 }
    });
    // A standby refuses changes and points at the leader.
    let (status, body, _) = call(
        "POST",
        &format!("{}/v1/schedules", b.url),
        Some(spec.clone()),
    )
    .unwrap();
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["code"], "NOT_LEADER");
    ok("POST", &format!("{}/v1/schedules", a.url), Some(spec));
    std::thread::sleep(Duration::from_millis(3_500));
    // Reads work on the standby, from the database.
    let s = ok("GET", &format!("{}/v1/schedules/tick", b.url), None);
    let fired = s["fired"].as_u64().unwrap();
    assert!(fired >= 2, "{s}");
    assert_eq!(metric(&b.url, "orbit_schedules_fired_total"), 0);
    assert!(metric(&a.url, "orbit_schedules_fired_total") >= 2);
    // Every occurrence was handled once: started or skipped.
    assert_fired_once(&a.url);

    // The leader goes away; the standby takes over and keeps firing.
    a.kill();
    assert_eq!(await_leader(&[&b], Duration::from_secs(10)), 0);
    std::thread::sleep(Duration::from_millis(3_000));
    assert!(metric(&b.url, "orbit_schedules_fired_total") >= 1);
    assert_fired_once(&b.url);
    // The new leader carried on from where the old one stopped.
    assert!(scheduled_ids(&b.url, "tick:").len() as u64 > fired);
}

#[test]
fn standby_takes_over_and_recovers_in_flight_work() {
    let db = db_or_skip!("cluster_failover");
    let mut a = TestNode::spawn(&db.url, "node-a");
    await_leader(&[&a], Duration::from_secs(10));
    let b = TestNode::spawn(&db.url, "node-b");
    assert_eq!(await_leader(&[&a, &b], Duration::from_secs(10)), 0);
    let term = a.node.term();
    ok(
        "POST",
        &format!("{}/v1/workflows", a.url),
        Some(serde_json::to_value(workflow()).unwrap()),
    );
    let reg = ok(
        "POST",
        &format!("{}/v1/workers/register", a.url),
        Some(
            json!({ "worker_id": "w", "runtime": "rust", "contracts": [{ "task_name": "a", "contract_version": "1.0.0" }] }),
        ),
    );
    let session = reg["worker_session_id"].as_str().unwrap().to_string();
    let mut tasks = vec![];
    for (id, n) in [("late", 1), ("lost", 2)] {
        ok(
            "POST",
            &format!("{}/v1/executions", a.url),
            Some(json!({ "workflow": "One", "input": { "n": n }, "execution_id": id })),
        );
        let t = ok(
            "POST",
            &format!("{}/v1/workers/poll", a.url),
            Some(json!({ "worker_session_id": session, "wait_ms": 5000 })),
        );
        assert_eq!(t["execution_id"], id);
        tasks.push(t);
    }

    // The leader goes away with both attempts in flight.
    let killed = Instant::now();
    a.kill();
    await_leader(&[&b], Duration::from_secs(10));
    let failover = killed.elapsed();
    assert!(
        failover < Duration::from_secs(5),
        "failover took {failover:?}"
    );
    assert_eq!(b.node.term(), term + 1, "the new leader starts a new term");

    // The worker's session belonged to the old leader: re-register.
    let (s, v, _) = call(
        "POST",
        &format!("{}/v1/workers/poll", b.url),
        Some(json!({ "worker_session_id": session, "wait_ms": 100 })),
    )
    .unwrap();
    assert_eq!(
        (s, v["error"]["code"].as_str()),
        (404, Some("UNKNOWN_SESSION"))
    );
    let reg = ok(
        "POST",
        &format!("{}/v1/workers/register", b.url),
        Some(serde_json::to_value(json!({ "worker_id": "w", "runtime": "rust", "contracts": [{ "task_name": "a", "contract_version": "1.0.0" }] })).unwrap()),
    );
    let session = reg["worker_session_id"].as_str().unwrap().to_string();

    // A late result with a token issued by the previous leader is accepted.
    let ack = ok(
        "POST",
        &format!("{}/v1/tasks/complete", b.url),
        Some(json!({ "token": tasks[0]["token"], "output": { "n": 10 } })),
    );
    assert_eq!(ack["accepted"], true, "{ack}");

    // The other attempt's outcome is uncertain: it is redelivered.
    let t = ok(
        "POST",
        &format!("{}/v1/workers/poll", b.url),
        Some(json!({ "worker_session_id": session, "wait_ms": 10000 })),
    );
    assert_eq!(t["execution_id"], "lost");
    assert_eq!(t["redelivery"], true);
    assert_eq!(t["idempotency_key"], tasks[1]["idempotency_key"]);
    ok(
        "POST",
        &format!("{}/v1/tasks/complete", b.url),
        Some(json!({ "token": t["token"], "output": { "n": 20 } })),
    );

    for (id, n) in [("late", 10), ("lost", 20)] {
        let s = ok(
            "GET",
            &format!("{}/v1/executions/{id}/wait?timeout_ms=10000", b.url),
            None,
        );
        assert_eq!(s["status"], "COMPLETED", "{s}");
        assert_eq!(s["output"]["n"], n);
    }
    let hist = ok(
        "GET",
        &format!("{}/v1/executions/lost/history", b.url),
        None,
    );
    let types: Vec<&str> = hist["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"StepAttemptUncertain"), "{types:?}");
    assert_eq!(
        types.iter().filter(|t| **t == "StepSucceeded").count(),
        1,
        "{types:?}"
    );
}

#[test]
fn a_deposed_leader_cannot_append() {
    let db = db_or_skip!("cluster_fencing");
    // Create the cluster table the way a node does, at term 1.
    let mut sql = db.sql();
    let open = |term: i64| {
        Engine::open(
            Store::Postgres(PgStore::connect_with(&db.url, PgMode::Fenced(term)).unwrap()),
            engine_config(),
            system_clock(),
        )
    };
    // The store creates its own schema; the cluster table comes from a node.
    let probe = TestNode::spawn(&db.url, "probe");
    await_leader(&[&probe], Duration::from_secs(10));
    drop(probe);
    let term: i64 = sql
        .query_one("SELECT term FROM orbit_cluster WHERE id = 1", &[])
        .unwrap()
        .get(0);

    let mut old = open(term).unwrap();
    old.register_definition(workflow()).unwrap();
    let session = old.register_worker(worker_req()).unwrap().worker_session_id;
    old.start("One", None, json!({ "n": 1 }), Some("f-1".into()))
        .unwrap();
    let task = old.poll(&session).unwrap().expect("a task");

    // Another server is elected: the term moves on.
    sql.execute("UPDATE orbit_cluster SET term = term + 1 WHERE id = 1", &[])
        .unwrap();
    let err = old.complete(&task.token, json!({ "n": 2 })).unwrap_err();
    assert_eq!(err.code, "STORAGE_ERROR");
    assert!(err.message.contains("fenced"), "{}", err.message);
    let err = old
        .start("One", None, json!({ "n": 3 }), Some("f-2".into()))
        .unwrap_err();
    assert!(err.message.contains("fenced"), "{}", err.message);

    // The new leader recovers the execution and accepts the late result.
    let mut new = open(term + 1).unwrap();
    assert_eq!(new.active_count(), 1);
    assert!(
        new.complete(&task.token, json!({ "n": 2 }))
            .unwrap()
            .accepted
    );
    assert_eq!(new.execution("f-1").unwrap().status.as_str(), "COMPLETED");
    assert!(
        new.execution("f-2").is_err(),
        "the fenced start left no trace"
    );
    // A read-only store never writes.
    let mut ro = Engine::open(
        Store::Postgres(PgStore::connect_with(&db.url, PgMode::ReadOnly).unwrap()),
        engine_config(),
        system_clock(),
    )
    .unwrap();
    assert!(ro.start("One", None, json!({ "n": 4 }), None).is_err());
    assert_eq!(ro.execution("f-1").unwrap().status.as_str(), "COMPLETED");
}

/// Every fire of `tick` started exactly one execution. The schedule keeps
/// firing every second, so it is read first and one more execution may have
/// started before the list is read.
fn assert_fired_once(url: &str) {
    let s = ok("GET", &format!("{url}/v1/schedules/tick"), None);
    let fired = s["fired"].as_u64().unwrap() as usize;
    let ids = scheduled_ids(url, "tick:");
    let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "{ids:?}");
    assert!(ids.len() == fired || ids.len() == fired + 1, "{s} {ids:?}");
    let last = s["last_execution_id"].as_str().unwrap().to_string();
    assert!(ids.contains(&last), "{s} {ids:?}");
}
