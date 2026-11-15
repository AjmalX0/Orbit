//! Multi-server operation: one active leader, any number of standbys, all on
//! the same PostgreSQL database (`orbit server --cluster`).
//!
//! * **Election.** Every node keeps a dedicated PostgreSQL session (separate
//!   from the store's connection) and tries `pg_try_advisory_lock` on a fixed
//!   key once per check interval. The session that holds the lock is the
//!   leader. PostgreSQL releases the lock when that session ends, so a crashed
//!   leader is replaced within about one interval.
//! * **Fencing.** On election the leader increments `orbit_cluster.term` and
//!   opens its store in [`PgMode::Fenced`] mode: every history append checks
//!   the term inside its own transaction. A deposed leader that has not yet
//!   noticed cannot write anything.
//! * **Liveness.** The leader re-checks its session, its lock and the term
//!   every interval. It only serves while the last successful check is younger
//!   than the lease; when a check fails it steps down to standby in-process
//!   (drops its engine and its lock session) and rejoins the election.
//! * **Standby.** A standby serves read-only routes from the database through
//!   an engine opened on a [`PgMode::ReadOnly`] store, and answers every write
//!   and worker route with `503 NOT_LEADER` and the leader's address.

use crate::engine::{Engine, EngineError};
use crate::server::{engine, Shared};
use crate::store::{session_client, PgMode, PgStore, Store, CLUSTER_SCHEMA};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Advisory lock key for leader election (ASCII "orbitldr").
pub const LEADER_LOCK_KEY: i64 = 0x6f72_6269_746c_6472;
/// Advisory lock key serialising creation of the cluster table.
const SCHEMA_LOCK_KEY: i64 = 0x6f72_6269_7473_6368;

/// Builds an engine on a store. The CLI passes one that applies the server's
/// engine configuration and telemetry, so leader and standby engines are
/// configured exactly like a single-node server's.
pub type EngineFactory =
    Box<dyn Fn(Store) -> std::result::Result<Engine, EngineError> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct ClusterConfig {
    /// PostgreSQL URL shared by every node.
    pub url: String,
    /// This node's name in health output, logs and `orbit_cluster`.
    pub node_id: String,
    /// URL clients and workers should use to reach this node directly.
    pub advertise_addr: Option<String>,
    /// How often a standby tries to take the lock and the leader re-checks it.
    pub check_interval: Duration,
    /// The leader stops serving when its last successful check is older than
    /// this. Must be shorter than the server-side session timeout below, so a
    /// partitioned leader stops before PostgreSQL releases its lock.
    pub lease: Duration,
    /// `idle_session_timeout` for the election session (PostgreSQL 14+): the
    /// server ends a leader session that stopped checking in, which releases
    /// the lock even when the leader host is unreachable.
    pub session_timeout: Duration,
}

impl ClusterConfig {
    pub fn new(url: &str, node_id: &str) -> ClusterConfig {
        ClusterConfig {
            url: url.to_string(),
            node_id: node_id.to_string(),
            advertise_addr: None,
            check_interval: Duration::from_secs(1),
            lease: Duration::from_secs(3),
            session_timeout: Duration::from_secs(5),
        }
    }
}

/// A default node id: `$HOSTNAME`, `/etc/hostname`, or a random id.
pub fn default_node_id() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| format!("node-{}", &uuid::Uuid::new_v4().to_string()[..8]))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Standby,
    Leader,
}

#[derive(Debug, Clone)]
struct Status {
    role: Role,
    /// The current term as last seen in `orbit_cluster`.
    term: i64,
    leader_node: Option<String>,
    leader_addr: Option<String>,
    /// Last successful leadership check (leader only).
    last_ok: Option<Instant>,
}

/// One cluster member. Created by the CLI, driven by [`start`].
pub struct Node {
    config: ClusterConfig,
    factory: EngineFactory,
    status: Mutex<Status>,
    stop: AtomicBool,
}

impl Node {
    pub fn new(config: ClusterConfig, factory: EngineFactory) -> Arc<Node> {
        Arc::new(Node {
            config,
            factory,
            status: Mutex::new(Status {
                role: Role::Standby,
                term: 0,
                leader_node: None,
                leader_addr: None,
                last_ok: None,
            }),
            stop: AtomicBool::new(false),
        })
    }

    pub fn config(&self) -> &ClusterConfig {
        &self.config
    }

    fn status(&self) -> std::sync::MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// True while this node leads and its leadership was confirmed within the
    /// lease. Checked on every request.
    pub fn is_leader(&self) -> bool {
        let s = self.status();
        s.role == Role::Leader && s.last_ok.is_some_and(|t| t.elapsed() < self.config.lease)
    }

    pub fn role(&self) -> Role {
        if self.is_leader() {
            Role::Leader
        } else {
            Role::Standby
        }
    }

    pub fn term(&self) -> i64 {
        self.status().term
    }

    /// The engine a standby serves reads from: recovers nothing, writes nothing.
    pub fn standby_engine(&self) -> std::result::Result<Engine, EngineError> {
        let store = PgStore::connect_with(&self.config.url, PgMode::ReadOnly)
            .map_err(|e| EngineError::new(500, "STORAGE_ERROR", e.to_string()))?;
        (self.factory)(Store::Postgres(store))
    }

    /// Stop taking part in the election (tests, embedding). A leader steps
    /// down at its next check and releases the lock.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn leader_json(&self) -> Value {
        let s = self.status();
        match &s.leader_node {
            Some(n) => json!({ "node_id": n, "addr": s.leader_addr }),
            None => Value::Null,
        }
    }

    /// Fields added to `/v1/health` and `/v1/ready`.
    fn health_fields(&self) -> Value {
        json!({
            "role": if self.is_leader() { "leader" } else { "standby" },
            "node_id": self.config.node_id,
            "term": self.term(),
            "leader": self.leader_json(),
        })
    }

    fn not_leader(&self) -> Response {
        let (node, addr) = {
            let s = self.status();
            (s.leader_node.clone(), s.leader_addr.clone())
        };
        let message = match (&node, &addr) {
            (Some(n), Some(a)) if *n != self.config.node_id => {
                format!("this server is a standby; the leader is `{n}` at {a}")
            }
            (Some(n), None) if *n != self.config.node_id => {
                format!("this server is a standby; the leader is `{n}`")
            }
            _ => "this server is a standby and no leader is serving yet; retry shortly".into(),
        };
        let mut resp = (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": {
                "code": "NOT_LEADER",
                "message": message,
                "leader_node": node,
                "leader_addr": addr,
            }})),
        )
            .into_response();
        let h = resp.headers_mut();
        h.insert("retry-after", HeaderValue::from_static("1"));
        if let Some(v) = addr.and_then(|a| HeaderValue::from_str(&a).ok()) {
            h.insert("x-orbit-leader", v);
        }
        resp
    }
}

// --------------------------------------------------------------- HTTP gate

#[derive(Clone)]
struct Gate {
    node: Option<Arc<Node>>,
    state: Shared,
}

/// Wrap the API router: adds `/v1/ready`, the cluster fields of
/// `/v1/health`, and (in cluster mode) refuses writes on a standby. Runs
/// before authentication, so load balancers can probe `/v1/ready` without
/// credentials.
pub fn layer(app: Router, node: Option<Arc<Node>>, state: Shared) -> Router {
    app.layer(middleware::from_fn_with_state(Gate { node, state }, gate))
}

/// Requests a standby answers itself (from the database).
fn is_read(method: &Method, path: &str) -> bool {
    (method == Method::GET || method == Method::HEAD) && path.starts_with("/v1/")
}

async fn gate(State(g): State<Gate>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if path == "/v1/ready" {
        let (ready, mut body) = match &g.node {
            None => (true, json!({ "role": "single" })),
            Some(n) => (n.is_leader(), n.health_fields()),
        };
        body["ready"] = json!(ready);
        let status = if ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        return (status, Json(body)).into_response();
    }
    if path == "/v1/health" {
        let resp = next.run(req).await;
        let fields = match &g.node {
            None => json!({ "role": "single", "node_id": null, "term": null, "leader": null }),
            Some(n) => n.health_fields(),
        };
        return merge_json(resp, fields).await;
    }
    let Some(node) = &g.node else {
        return next.run(req).await;
    };
    if node.is_leader() {
        return next.run(req).await;
    }
    if is_read(req.method(), &path) {
        // Definitions registered on the leader since this standby opened.
        if path.starts_with("/v1/workflows") || path.contains("/queries/") {
            let st = g.state.clone();
            let _ = tokio::task::spawn_blocking(move || engine(&st).refresh_definitions()).await;
        }
        return next.run(req).await;
    }
    node.not_leader()
}

/// Add `fields` to a JSON object response.
async fn merge_json(resp: Response, fields: Value) -> Response {
    if resp.status() != StatusCode::OK {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 1 << 20).await else {
        return Response::from_parts(parts, Body::empty());
    };
    let mut v: Value = match serde_json::from_slice(&bytes) {
        Ok(v @ Value::Object(_)) => v,
        _ => return Response::from_parts(parts, Body::from(bytes)),
    };
    if let (Some(obj), Value::Object(extra)) = (v.as_object_mut(), fields) {
        obj.extend(extra);
    }
    let out = serde_json::to_vec(&v).unwrap_or_default();
    parts.headers.remove("content-length");
    Response::from_parts(parts, Body::from(out))
}

/// gRPC interceptor: on a standby every worker call fails with `UNAVAILABLE`
/// and `orbit-error-code: NOT_LEADER` (SDKs retry it like a connection error).
pub fn grpc_gate(
    node: Arc<Node>,
) -> impl FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Clone {
    move |req| {
        if node.is_leader() {
            return Ok(req);
        }
        let addr = node.status().leader_addr.clone();
        let mut s = tonic::Status::unavailable(match &addr {
            Some(a) => format!("NOT_LEADER: this server is a standby; the leader is at {a}"),
            None => "NOT_LEADER: this server is a standby".to_string(),
        });
        s.metadata_mut()
            .insert("orbit-error-code", "NOT_LEADER".parse().expect("ascii"));
        if let Some(v) = addr.and_then(|a| a.parse().ok()) {
            s.metadata_mut().insert("x-orbit-leader", v);
        }
        Err(s)
    }
}

// ---------------------------------------------------------------- election

/// Start the election thread for `node`, swapping engines in `state` as the
/// node gains and loses leadership. `state` must hold a standby engine
/// ([`Node::standby_engine`]) to begin with.
pub fn start(node: Arc<Node>, state: Shared) -> std::io::Result<std::thread::JoinHandle<()>> {
    tracing::info!(
        node_id = %node.config.node_id,
        advertise_addr = node.config.advertise_addr.as_deref().unwrap_or(""),
        "cluster mode: standby until elected"
    );
    std::thread::Builder::new()
        .name("orbit-cluster".into())
        .spawn(move || election_loop(&node, &state))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn connect(config: &ClusterConfig) -> Result<postgres::Client, String> {
    let mut c = session_client(&config.url, &format!("orbit-cluster:{}", config.node_id))
        .map_err(|e| e.to_string())?;
    // Older servers lack idle_session_timeout; TCP keepalives still apply.
    let _ = c.batch_execute(&format!(
        "SET idle_session_timeout = '{}ms'",
        config.session_timeout.as_millis()
    ));
    c.batch_execute(&format!(
        "SET statement_timeout = '{}ms'",
        config.lease.as_millis()
    ))
    .map_err(|e| e.to_string())?;
    let mut tx = c.transaction().map_err(|e| e.to_string())?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK_KEY])
        .map_err(|e| e.to_string())?;
    let exists: bool = tx
        .query_one("SELECT to_regclass('orbit_cluster') IS NOT NULL", &[])
        .map_err(|e| e.to_string())?
        .get(0);
    if !exists {
        tx.batch_execute(CLUSTER_SCHEMA)
            .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(c)
}

fn sleep_interval(node: &Node) {
    std::thread::sleep(node.config.check_interval);
}

fn election_loop(node: &Node, state: &Shared) {
    let mut reader_ok = true;
    let mut followed: Option<(i64, Option<String>)> = None;
    while !node.stopped() {
        let mut client = match connect(&node.config) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "cluster: cannot reach PostgreSQL for leader election; retrying");
                sleep_interval(node);
                continue;
            }
        };
        // Standby: try to take the lock, otherwise follow the current leader.
        let elected = loop {
            if node.stopped() {
                return;
            }
            if !reader_ok {
                reader_ok = install_standby_engine(node, state);
            }
            match client.query_one("SELECT pg_try_advisory_lock($1)", &[&LEADER_LOCK_KEY]) {
                Ok(r) if r.get::<_, bool>(0) => break true,
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "cluster: election session failed; reconnecting");
                    break false;
                }
            }
            match client.query_one(
                "SELECT term, leader_node, leader_addr FROM orbit_cluster WHERE id = 1",
                &[],
            ) {
                Ok(r) => {
                    let (term, leader, addr): (i64, Option<String>, Option<String>) =
                        (r.get(0), r.get(1), r.get(2));
                    if followed.as_ref() != Some(&(term, leader.clone())) {
                        tracing::info!(
                            term,
                            leader = leader.as_deref().unwrap_or("none"),
                            "cluster: standby; following the leader"
                        );
                        followed = Some((term, leader.clone()));
                    }
                    let mut s = node.status();
                    s.term = term;
                    s.leader_node = leader;
                    s.leader_addr = addr;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "cluster: election session failed; reconnecting");
                    break false;
                }
            }
            // Long-polls (`/wait`) on a standby re-read the database.
            state.changed.notify_waiters();
            sleep_interval(node);
        };
        if !elected {
            sleep_interval(node);
            continue;
        }
        let reason = lead(node, state, &mut client);
        // Step down: stop serving first, then drop the leader engine.
        {
            let mut s = node.status();
            s.role = Role::Standby;
            s.last_ok = None;
        }
        tracing::error!(node_id = %node.config.node_id, reason = %reason, "cluster: lost leadership; stepped down to standby");
        drop(client); // ends the session, releasing the lock if still held
        reader_ok = install_standby_engine(node, state);
        followed = None;
        if !node.stopped() {
            sleep_interval(node);
        }
    }
}

/// Replace the engine with a standby engine. If the database is unreachable,
/// install an empty in-memory engine so nothing of the old leader keeps
/// running, and report failure so the caller retries.
fn install_standby_engine(node: &Node, state: &Shared) -> bool {
    let (engine_new, ok) = match node.standby_engine() {
        Ok(e) => (e, true),
        Err(e) => {
            tracing::warn!(error = %e, "cluster: cannot open the standby store; retrying");
            match Store::in_memory()
                .map_err(|e| e.to_string())
                .and_then(|s| (node.factory)(s).map_err(|e| e.to_string()))
            {
                Ok(e) => (e, false),
                Err(_) => return false,
            }
        }
    };
    swap_engine(state, engine_new);
    ok
}

fn swap_engine(state: &Shared, mut new: Engine) {
    new.changed = state.changed.clone();
    let old = std::mem::replace(&mut *engine(state), new);
    drop(old);
    state.changed.notify_waiters();
}

/// Take office, then hold it until a check fails. Returns why it ended.
fn lead(node: &Node, state: &Shared, client: &mut postgres::Client) -> String {
    let addr = node.config.advertise_addr.clone();
    let term: i64 = match client.query_one(
        "UPDATE orbit_cluster SET term = term + 1, leader_node = $1, leader_addr = $2, since_ms = $3 WHERE id = 1 RETURNING term",
        &[&node.config.node_id, &addr, &now_ms()],
    ) {
        Ok(r) => r.get(0),
        Err(e) => return format!("cannot record the new term: {e}"),
    };
    {
        let mut s = node.status();
        s.term = term;
        s.leader_node = Some(node.config.node_id.clone());
        s.leader_addr = addr.clone();
    }
    tracing::info!(node_id = %node.config.node_id, term, "cluster: elected leader; recovering executions from history");
    let started = Instant::now();
    // Recovery may take longer than the idle timeout of this (idle) session;
    // suspend it meanwhile so the lock is not released under our feet.
    let _ = client.batch_execute("SET idle_session_timeout = 0");
    let opened = PgStore::connect_with(&node.config.url, PgMode::Fenced(term))
        .map_err(|e| e.to_string())
        .and_then(|s| (node.factory)(Store::Postgres(s)).map_err(|e| e.to_string()));
    let _ = client.batch_execute(&format!(
        "SET idle_session_timeout = '{}ms'",
        node.config.session_timeout.as_millis()
    ));
    let leader_engine = match opened {
        Ok(e) => e,
        Err(e) => return format!("cannot open the engine: {e}"),
    };
    let active = leader_engine.active_count();
    swap_engine(state, leader_engine);
    {
        let mut s = node.status();
        s.role = Role::Leader;
        s.last_ok = Some(Instant::now());
    }
    tracing::info!(node_id = %node.config.node_id, term, active_executions = active, recovery_ms = started.elapsed().as_millis() as u64, "cluster: leader is serving");
    loop {
        sleep_interval(node);
        if node.stopped() {
            return "node is shutting down".into();
        }
        let check = client.query_one(
            "SELECT (SELECT term FROM orbit_cluster WHERE id = 1),
                    EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' AND pid = pg_backend_pid() AND granted)",
            &[],
        );
        match check {
            Ok(r) => {
                let (current, holds): (Option<i64>, bool) = (r.get(0), r.get(1));
                if !holds {
                    return "the election lock is no longer held".into();
                }
                if current != Some(term) {
                    return format!(
                        "the cluster moved to term {} (this node led term {term})",
                        current.unwrap_or(-1)
                    );
                }
                // A slow check only pauses serving (the lease lapsed); the
                // lock and the term prove nobody else took over meanwhile.
                node.status().last_ok = Some(Instant::now());
            }
            Err(e) => return format!("the election session failed: {e}"),
        }
    }
}
