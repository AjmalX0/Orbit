//! Shared mode: several active servers on one PostgreSQL database
//! (`orbit server --cluster --cluster-mode shared`). Every node serves every
//! route; executions, worker tasks and worker sessions are spread over them.
//!
//! * **Membership.** Every node keeps a row in `orbit_nodes` and refreshes
//!   its heartbeat (database clock) every second. A node whose heartbeat is
//!   older than the node timeout is dead.
//! * **Ownership.** Each running execution has one owner node and an epoch in
//!   `orbit_owners`. The node that starts an execution owns it (epoch 1);
//!   child workflows share their root's ownership. Only the owner appends to
//!   an execution's history, and every append checks the epoch with
//!   `SELECT ... FOR SHARE` in its own transaction, as the active/standby
//!   cluster checks its term. Live nodes take over a dead node's executions
//!   with `UPDATE ... SET epoch = epoch + 1 WHERE node_id = <dead>` (one winner
//!   per row), spread by rendezvous hashing, and rebuild them from history.
//! * **Task queue.** The owner writes a row to `orbit_tasks` for every worker
//!   task it schedules, in the same transaction as the `StepScheduled` event.
//!   Any node claims rows for the workers polling it (`FOR UPDATE SKIP
//!   LOCKED`), returns the assignment at once, and tells the owner through
//!   the owner's inbox, so the owner remains the only writer of the history.
//! * **Inbox.** Commands for an execution owned elsewhere (dispatches made by
//!   other nodes, results, signals, cancellations) go to `orbit_inbox`. The
//!   owner applies them in order through the ordinary engine methods and
//!   deletes each entry in the same transaction as the events it caused.
//! * **Worker sessions** are stored in `orbit_sessions`, so a worker can poll
//!   and report through any node, and keep working when a node dies.
//!
//! See `docs/src/runtime/cluster.md` ("Shared mode") for the full picture.

use crate::engine::{ContractAnnouncement, Engine, EngineError, TaskAssignment, WorkerInfo};
use crate::model::{is_cleanup_key, Event, ExecutionState, HistoryEvent, UnitStatus};
use crate::server::{engine, Shared};
use crate::store::{
    session_client, PgMode, PgStore, SharedAppend, Store, StoreError, TaskOffer, UnitSync,
    WorkerRecord, NOW_MS, SHARED_SCHEMA,
};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use orbit_ir::{Invocation, WorkflowIr};
use postgres::fallible_iterator::FallibleIterator;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Advisory lock key serialising creation of the shared-mode tables.
const SCHEMA_LOCK_KEY: i64 = 0x6f72_6269_7473_6873;
/// How much later than on one server a worker session that went quiet is
/// declared lost: its last contact reaches the owner through the database.
const SESSION_LAG_MS: u64 = 1_000;
/// Connections for request-path statements per server.
const OPS_CONNECTIONS: usize = 4;
/// How long a signal or cancel sent through a non-owner waits for the owner.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// A session that made no claim since the queue last changed re-queries
/// the database at most this often.
const REQUERY: Duration = Duration::from_secs(1);

/// Builds a node's engine on its store (as for [`crate::cluster`]).
pub type EngineFactory = crate::cluster::EngineFactory;

#[derive(Debug, Clone)]
pub struct SharedConfig {
    /// PostgreSQL URL shared by every node.
    pub url: String,
    /// This node's name in `orbit_nodes`, health output and logs. Must be
    /// unique among running servers.
    pub node_id: String,
    /// URL clients and workers can use to reach this node directly.
    pub advertise_addr: Option<String>,
    /// How often the node refreshes its heartbeat and looks for dead nodes.
    pub heartbeat_interval: Duration,
    /// A node whose heartbeat is older than this is dead; its executions are
    /// taken over.
    pub node_timeout: Duration,
    /// Executions trees taken over per round and node.
    pub takeover_batch: i64,
}

impl SharedConfig {
    pub fn new(url: &str, node_id: &str) -> SharedConfig {
        SharedConfig {
            url: url.to_string(),
            node_id: node_id.to_string(),
            advertise_addr: None,
            heartbeat_interval: Duration::from_secs(1),
            node_timeout: Duration::from_secs(5),
            takeover_batch: 100,
        }
    }
}

/// Outcome of a claim on the shared queue.
pub(crate) enum Claimed {
    /// Nothing for this worker.
    Nothing,
    /// A row whose execution finished meanwhile; claim again.
    Stale,
    /// A task of an execution another node owns (the owner was told).
    Remote(TaskAssignment),
    /// A task of an execution this node owns: record the dispatch locally.
    Local {
        row_id: i64,
        offer: u32,
        assignment: TaskAssignment,
    },
}

struct Staged {
    seq: i64,
    execution_id: String,
    kind: String,
    consumed: bool,
}

/// Shared-mode state of one node, held by its engine and its threads.
pub struct Ctx {
    config: SharedConfig,
    incarnation: String,
    session_timeout_ms: u64,
    /// Request-path statements (claims, forwarding, sessions), spread over
    /// a few connections so that concurrent polls do not queue on one.
    pool: Vec<PgStore>,
    next: std::sync::atomic::AtomicUsize,
    /// Ownership epochs of the executions this node holds.
    epochs: Arc<Mutex<HashMap<String, i64>>>,
    /// Inbox entries being applied: consumed by the next append for their
    /// execution, in the same transaction.
    staged: Mutex<Vec<Staged>>,
    /// (execution, token) whose result arrived: their queue rows become done.
    done: Mutex<Vec<(String, String)>>,
    /// Executions whose append was fenced: another node owns them now.
    fenced: Mutex<HashSet<String>>,
    /// Sessions touched since the last flush to `orbit_sessions`.
    touched: Mutex<HashMap<String, Instant>>,
    /// Last contact of sessions holding attempts this node owns: age when
    /// read and when it was read; `None` when the session no longer exists.
    seen: Mutex<HashMap<String, Option<(u64, Instant)>>>,
    /// Tokens of attempts handed out by a server that died (or restarted)
    /// more than a session timeout ago without their result arriving.
    lost: Mutex<HashSet<String>>,
    /// Bumped whenever the queue may have new rows.
    queue_gen: AtomicU64,
    /// Per session: the queue generation and time of its last empty claim.
    empty: Mutex<HashMap<String, (u64, Instant)>>,
    defs_dirty: AtomicBool,
    applying: AtomicBool,
    started: AtomicBool,
    duplicate: AtomicBool,
    last_beat: Mutex<Option<Instant>>,
    nodes: Mutex<Value>,
    tasks_claimed: AtomicU64,
    stop: AtomicBool,
    halt: AtomicBool,
    /// Set by [`start`]; the control thread then sets `exited` when it leaves.
    spawned: AtomicBool,
    /// Set by the control thread when it has left (released this node).
    exited: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn storage(e: StoreError) -> EngineError {
    EngineError::new(500, "STORAGE_ERROR", e.to_string())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn error_json(status: u16, code: &str, message: &str) -> String {
    json!({ "error": { "status": status, "code": code, "message": message } }).to_string()
}

tokio::task_local! {
    static FORWARDED: Arc<Mutex<Option<i64>>>;
}

/// Remember, for the HTTP request being served, that its command was sent to
/// the owner's inbox as entry `seq` (the gate then waits for the owner).
pub(crate) fn note_forwarded(seq: i64) {
    let _ = FORWARDED.try_with(|s| *lock(s) = Some(seq));
}

/// The execution a shared-mode task token belongs to (`<execution>/<uuid>`).
pub(crate) fn token_execution(token: &str) -> Option<&str> {
    token
        .rsplit_once('/')
        .map(|(e, _)| e)
        .filter(|e| !e.is_empty())
}

/// The invocation of a unit that runs on a worker (not a built-in task, not a
/// child workflow): these are the units that go on the shared queue.
pub(crate) fn worker_invocation<'a>(def: &'a WorkflowIr, key: &str) -> Option<&'a Invocation> {
    let inv = crate::decide::invocation(def, key)?;
    match def.contracts.get(&inv.task) {
        Some(c) if c.builtin || c.workflow_hash.is_some() => None,
        _ => Some(inv),
    }
}

/// The queue row for a scheduled worker unit.
pub(crate) fn offer_for(state: &ExecutionState, key: &str, def: &WorkflowIr) -> Option<TaskOffer> {
    let inv = worker_invocation(def, key)?;
    let u = state.units.get(key)?;
    let a = TaskAssignment {
        token: String::new(),
        execution_id: state.id.clone(),
        workflow: def.name.clone(),
        workflow_version: def.version.clone(),
        step_id: key.to_string(),
        task_name: inv.task.clone(),
        contract_version: inv.contract_version.clone(),
        input: u.input.clone().unwrap_or(Value::Null),
        idempotency_key: u.idempotency_key.clone().unwrap_or_default(),
        attempt: u.attempt,
        redelivery: u.redelivery,
        deadline_ms: None,
        timeout_ms: inv.timeout_ms,
    };
    Some(TaskOffer {
        unit_key: key.to_string(),
        attempt: u.attempt as i64,
        offer: u.dispatch_count as i64,
        task_name: inv.task.clone(),
        contract_version: inv.contract_version.clone(),
        language: inv.language.as_ref().map(|l| l.to_lowercase()),
        payload: serde_json::to_string(&a).expect("assignment serialises"),
        enqueued_ms: u.scheduled_at_ms.unwrap_or(0) as i64,
    })
}

/// Queue changes caused by a batch of events: new offers for units that were
/// scheduled, and withdrawals for units that moved on.
fn queue_changes(
    state: &ExecutionState,
    history: &[HistoryEvent],
    def: &WorkflowIr,
) -> (Vec<TaskOffer>, Vec<UnitSync>) {
    let aborting = history.iter().any(|h| {
        matches!(
            h.event,
            Event::CancelRequested { .. }
                | Event::WorkflowTimeoutReached { .. }
                | Event::StepFailed { .. }
                | Event::LimitExceeded { .. }
                | Event::CompensationStarted { .. }
                | Event::HandlersStarted { .. }
        )
    });
    // unit -> (scheduled in this batch, needs a withdrawal check)
    let mut touched: BTreeMap<&str, (bool, bool)> = BTreeMap::new();
    for h in history {
        if let Some(step) = h.event.step() {
            let e = touched.entry(step).or_default();
            match h.event {
                Event::StepScheduled { .. } => e.0 = true,
                Event::StepDispatched { .. } => {}
                _ => e.1 = true,
            }
        }
    }
    if aborting {
        for (key, u) in &state.units {
            if matches!(u.status(), UnitStatus::Scheduled | UnitStatus::Dispatched) {
                touched.entry(key.as_str()).or_default().1 = true;
            }
        }
    }
    let mut offers = vec![];
    let mut syncs = vec![];
    for (key, (scheduled, check)) in touched {
        if worker_invocation(def, key).is_none() {
            continue;
        }
        let Some(u) = state.units.get(key) else {
            continue;
        };
        if scheduled && u.status() == UnitStatus::Scheduled {
            offers.extend(offer_for(state, key, def));
        }
        if check {
            let keep = match u.status() {
                UnitStatus::Scheduled => Some((u.attempt as i64, u.dispatch_count as i64)),
                UnitStatus::Dispatched if state.abort.is_none() || is_cleanup_key(key) => {
                    Some((u.attempt as i64, u.dispatch_count as i64 - 1))
                }
                _ => None,
            };
            syncs.push(UnitSync {
                unit_key: key.to_string(),
                keep,
            });
        }
    }
    (offers, syncs)
}

impl Ctx {
    fn ops(&self) -> &PgStore {
        &self.pool[self.next.fetch_add(1, Ordering::Relaxed) % self.pool.len()]
    }

    pub fn node_id(&self) -> &str {
        &self.config.node_id
    }

    /// Number of executions this node owns.
    pub fn owned_executions(&self) -> usize {
        lock(&self.epochs).len()
    }

    /// Tasks claimed from the shared queue by this node since it started.
    pub fn tasks_claimed(&self) -> u64 {
        self.tasks_claimed.load(Ordering::Relaxed)
    }

    /// Leave the cluster on shutdown: other nodes take over at once. Waits
    /// (up to five seconds) until the node's release is written.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if !self.spawned.load(Ordering::SeqCst) {
            // `start` never ran (an engine that was not served).
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.exited.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Freeze this node as if its process hung: no heartbeats, no takeovers,
    /// no inbox processing, and no release on shutdown (tests).
    pub fn halt(&self) {
        self.halt.store(true, Ordering::SeqCst);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn halted(&self) -> bool {
        self.halt.load(Ordering::SeqCst)
    }

    /// Serving: started, heartbeat confirmed within the node timeout.
    pub fn is_ready(&self) -> bool {
        self.started.load(Ordering::SeqCst)
            && !self.duplicate.load(Ordering::SeqCst)
            && !self.halted()
            && lock(&self.last_beat).is_some_and(|t| t.elapsed() < self.config.node_timeout)
    }

    pub(crate) fn applying_inbox(&self) -> bool {
        self.applying.load(Ordering::SeqCst)
    }

    pub(crate) fn set_epoch(&self, id: &str, epoch: i64) {
        lock(&self.epochs).insert(id.to_string(), epoch);
    }

    pub(crate) fn forget(&self, id: &str) {
        lock(&self.epochs).remove(id);
    }

    pub(crate) fn stage_done(&self, execution_id: &str, token: &str) {
        lock(&self.done).push((execution_id.to_string(), token.to_string()));
    }

    fn stage(&self, seq: i64, execution_id: &str, kind: &str) {
        lock(&self.staged).push(Staged {
            seq,
            execution_id: execution_id.to_string(),
            kind: kind.to_string(),
            consumed: false,
        });
    }

    /// Remove a staged entry; true when an append consumed it.
    fn unstage(&self, seq: i64) -> bool {
        let mut s = lock(&self.staged);
        let consumed = s.iter().any(|e| e.seq == seq && e.consumed);
        s.retain(|e| e.seq != seq);
        consumed
    }

    /// History append for an owned (or new) execution, with the queue and
    /// inbox changes it implies, in one transaction.
    pub(crate) fn append(
        &self,
        store: &mut Store,
        state: &ExecutionState,
        history: &[HistoryEvent],
        def: Option<&WorkflowIr>,
        rollover: Option<(u32, Vec<HistoryEvent>)>,
    ) -> crate::store::Result<()> {
        let Store::Postgres(pg) = store else {
            return Err(StoreError("shared mode needs a PostgreSQL store".into()));
        };
        let id = state.id.clone();
        // A new run (continue-as-new) also starts at 1 but keeps its owner.
        let created = rollover.is_none() && history.first().is_some_and(|h| h.seq == 1);
        let epoch = if created {
            None
        } else {
            match lock(&self.epochs).get(&id).copied() {
                Some(e) => Some(e),
                None => {
                    return Err(StoreError(format!(
                        "fenced: this server does not own execution `{id}`"
                    )))
                }
            }
        };
        let (offers, syncs) = match def {
            Some(d) => queue_changes(state, history, d),
            None => (vec![], vec![]),
        };
        let mut consumed = vec![];
        {
            let signal_seq = history
                .iter()
                .find_map(|h| matches!(h.event, Event::SignalReceived { .. }).then_some(h.seq));
            for e in lock(&self.staged).iter_mut() {
                if e.execution_id == id && !e.consumed {
                    e.consumed = true;
                    let reply = match e.kind.as_str() {
                        "signal" => Some(json!({ "seq": signal_seq }).to_string()),
                        "cancel" => Some(json!({ "ok": true }).to_string()),
                        _ => None,
                    };
                    consumed.push((e.seq, reply));
                }
            }
        }
        let done_tokens: Vec<String> = {
            let mut d = lock(&self.done);
            let (mine, rest): (Vec<_>, Vec<_>) = d.drain(..).partition(|(e, _)| *e == id);
            *d = rest;
            mine.into_iter().map(|(_, t)| t).collect()
        };
        let terminal = state.status.is_terminal();
        let ext = SharedAppend {
            node_id: self.config.node_id.clone(),
            epoch,
            parent: state.parent.as_ref().map(|p| p.execution_id.clone()),
            offers,
            syncs,
            done_tokens,
            consumed: consumed.clone(),
            finished_reply: terminal.then(|| {
                error_json(
                    409,
                    "EXECUTION_FINISHED",
                    &format!("execution is already {}", state.status.as_str()),
                )
            }),
            rollover,
        };
        let result = pg.append_shared(state, history, ext);
        match &result {
            Ok(()) => {
                let mut epochs = lock(&self.epochs);
                if terminal {
                    epochs.remove(&id);
                } else if created {
                    epochs.insert(id, 1);
                }
            }
            Err(e) => {
                let seqs: HashSet<i64> = consumed.iter().map(|(s, _)| *s).collect();
                for e in lock(&self.staged).iter_mut() {
                    if seqs.contains(&e.seq) {
                        e.consumed = false;
                    }
                }
                if e.0.starts_with("fenced") && !created {
                    lock(&self.fenced).insert(id);
                }
            }
        }
        result
    }

    /// Whether a poll from `session` should query the queue now.
    pub(crate) fn should_claim(&self, session: &str) -> bool {
        let gen = self.queue_gen.load(Ordering::SeqCst);
        lock(&self.empty)
            .get(session)
            .is_none_or(|(g, at)| *g != gen || at.elapsed() >= REQUERY)
    }

    /// Claim the oldest queued task matching a worker's contracts and runtime.
    pub(crate) fn claim(
        &self,
        session: &str,
        worker_id: &str,
        runtime: &str,
        contracts: &HashSet<(String, String)>,
        now: u64,
    ) -> Result<Claimed, EngineError> {
        let gen = self.queue_gen.load(Ordering::SeqCst);
        let (names, versions): (Vec<String>, Vec<String>) = contracts.iter().cloned().unzip();
        let suffix = uuid::Uuid::new_v4().to_string();
        let me = self.config.node_id.clone();
        let epochs = self.epochs.clone();
        let incarnation = self.incarnation.clone();
        let (session_id, worker, runtime) = (
            session.to_string(),
            worker_id.to_string(),
            runtime.to_lowercase(),
        );
        let now = now as i64;
        let claimed = self
            .ops()
            .run(move |c| {
                let mut tx = c.transaction()?;
                let Some(row) = tx.query_opt(
                    "WITH c AS (
                         SELECT id FROM orbit_tasks
                         WHERE state = 'queued'
                           AND (task_name, contract_version) IN (SELECT n, v FROM unnest($1::text[], $2::text[]) AS u(n, v))
                           AND (language IS NULL OR language = 'any' OR language = $3)
                         ORDER BY enqueued_ms, id
                         LIMIT 1
                         FOR UPDATE SKIP LOCKED)
                     UPDATE orbit_tasks t
                     SET state = 'claimed', token = t.execution_id || '/' || $4, claimed_by = $5, claimed_inc = $9,
                         claimed_session = $6, claimed_worker = $7, claimed_ms = $8, updated_ms = $8
                     FROM c WHERE t.id = c.id
                     RETURNING t.id, t.execution_id, t.unit_key, t.attempt, t.offer, t.payload, t.token",
                    &[&names, &versions, &runtime, &suffix, &me, &session_id, &worker, &now, &incarnation],
                )?
                else {
                    return Ok(None);
                };
                let (row_id, exec, unit, attempt, offer, payload, token): (
                    i64,
                    String,
                    String,
                    i64,
                    i64,
                    String,
                    String,
                ) = (
                    row.get(0),
                    row.get(1),
                    row.get(2),
                    row.get(3),
                    row.get(4),
                    row.get(5),
                    row.get(6),
                );
                let mut a: TaskAssignment = serde_json::from_str(&payload)
                    .map_err(|e| StoreError(format!("unreadable queued task: {e}")))?;
                a.token = token.clone();
                a.deadline_ms = a.timeout_ms.map(|t| now as u64 + t);
                let owner: Option<(String, i64)> = tx
                    .query_opt(
                        "SELECT node_id, epoch FROM orbit_owners WHERE execution_id = $1 FOR SHARE",
                        &[&exec],
                    )?
                    .map(|r| (r.get(0), r.get(1)));
                let mine = lock(&epochs).get(&exec).copied();
                let local = matches!((&owner, mine), (Some((n, e)), Some(m)) if *n == me && *e == m);
                let outcome = match &owner {
                    _ if local => Claimed::Local {
                        row_id,
                        offer: offer as u32,
                        assignment: a,
                    },
                    None => {
                        tx.execute(
                            "UPDATE orbit_tasks SET state = 'cancelled', updated_ms = $2 WHERE id = $1",
                            &[&row_id, &now],
                        )?;
                        Claimed::Stale
                    }
                    Some((owner_node, _)) => {
                        let msg = json!({
                            "unit": unit,
                            "attempt": attempt,
                            "offer": offer,
                            "token": token,
                            "worker_id": worker,
                            "session": session_id,
                            "deadline_ms": a.deadline_ms,
                        });
                        tx.execute(
                            "INSERT INTO orbit_inbox (execution_id, kind, token, payload, created_ms) VALUES ($1, 'dispatched', $2, $3, $4)",
                            &[&exec, &token, &msg.to_string(), &now],
                        )?;
                        tx.execute("SELECT pg_notify('orbit_inbox', $1)", &[owner_node])?;
                        Claimed::Remote(a)
                    }
                };
                tx.commit()?;
                Ok(Some(outcome))
            })
            .map_err(storage)?;
        match claimed {
            None => {
                lock(&self.empty).insert(session.to_string(), (gen, Instant::now()));
                Ok(Claimed::Nothing)
            }
            Some(c) => {
                lock(&self.empty).remove(session);
                if !matches!(c, Claimed::Stale) {
                    self.tasks_claimed.fetch_add(1, Ordering::Relaxed);
                }
                Ok(c)
            }
        }
    }

    /// Withdraw a claimed row whose unit no longer waits for it.
    pub(crate) fn retire(&self, row_id: i64, token: &str) -> Result<(), EngineError> {
        let token = token.to_string();
        self.ops()
            .run(move |c| {
                c.execute(
                    "UPDATE orbit_tasks SET state = 'cancelled', updated_ms = $3 WHERE id = $1 AND token = $2",
                    &[&row_id, &token, &now_ms()],
                )?;
                Ok(())
            })
            .map_err(storage)
    }

    /// Put a claimed row back on the queue (the local dispatch failed).
    pub(crate) fn release(&self, row_id: i64, token: &str) -> Result<(), EngineError> {
        let token = token.to_string();
        self.ops()
            .run(move |c| {
                c.execute(
                    "UPDATE orbit_tasks SET state = 'queued', token = NULL, claimed_by = NULL, claimed_inc = NULL,
                         claimed_session = NULL, claimed_worker = NULL, claimed_ms = NULL, updated_ms = $3
                     WHERE id = $1 AND token = $2 AND state = 'claimed'",
                    &[&row_id, &token, &now_ms()],
                )?;
                c.execute("SELECT pg_notify('orbit_tasks', '')", &[])?;
                Ok(())
            })
            .map_err(storage)
    }

    /// Send a command to the owner of `execution_id` through its inbox.
    /// Returns the inbox sequence number, or `None` when the execution has no
    /// owner any more (it finished). A result (`token` set) also marks its
    /// queue row done.
    pub(crate) fn forward(
        &self,
        execution_id: &str,
        kind: &str,
        payload: Value,
        token: Option<&str>,
    ) -> Result<Option<i64>, EngineError> {
        let (exec, kind, token) = (
            execution_id.to_string(),
            kind.to_string(),
            token.map(str::to_string),
        );
        let payload = payload.to_string();
        self.ops()
            .run(move |c| {
                let mut tx = c.transaction()?;
                let Some(owner) = tx
                    .query_opt(
                        "SELECT node_id FROM orbit_owners WHERE execution_id = $1",
                        &[&exec],
                    )?
                    .map(|r| r.get::<_, String>(0))
                else {
                    return Ok(None);
                };
                let now = now_ms();
                if let Some(t) = &token {
                    tx.execute(
                        "UPDATE orbit_tasks SET state = 'done', updated_ms = $2 WHERE token = $1 AND state IN ('claimed', 'cancelled')",
                        &[t, &now],
                    )?;
                }
                let seq: i64 = tx
                    .query_one(
                        "INSERT INTO orbit_inbox (execution_id, kind, token, payload, created_ms) VALUES ($1, $2, $3, $4, $5) RETURNING seq",
                        &[&exec, &kind, &token, &payload, &now],
                    )?
                    .get(0);
                tx.execute("SELECT pg_notify('orbit_inbox', $1)", &[&owner])?;
                tx.commit()?;
                Ok(Some(seq))
            })
            .map_err(storage)
    }

    /// Wait for the owner to apply inbox entry `seq`; its reply, or `None`
    /// on timeout.
    fn wait_reply(&self, seq: i64, timeout: Duration) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let r = self.ops().run(move |c| {
                Ok(c.query_opt(
                    "SELECT done_ms IS NOT NULL, result FROM orbit_inbox WHERE seq = $1",
                    &[&seq],
                )?
                .map(|r| (r.get::<_, bool>(0), r.get::<_, Option<String>>(1))))
            });
            match r {
                Ok(Some((true, result))) => {
                    return Some(
                        result
                            .and_then(|r| serde_json::from_str(&r).ok())
                            .unwrap_or(Value::Null),
                    )
                }
                Ok(None) => return Some(Value::Null),
                _ => {}
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Record a new worker session for every node.
    pub(crate) fn save_session(
        &self,
        session_id: &str,
        worker_id: &str,
        runtime: &str,
        contracts: &[ContractAnnouncement],
    ) -> Result<(), EngineError> {
        let (sid, wid, rt, node) = (
            session_id.to_string(),
            worker_id.to_string(),
            runtime.to_lowercase(),
            self.config.node_id.clone(),
        );
        let contracts = serde_json::to_string(contracts).expect("contracts serialise");
        self.ops()
            .run(move |c| {
                c.execute(
                    &format!(
                        "INSERT INTO orbit_sessions (session_id, worker_id, runtime, contracts, node_id, registered_ms, last_seen_ms)
                         VALUES ($1, $2, $3, $4, $5, {NOW_MS}, {NOW_MS}) ON CONFLICT DO NOTHING"
                    ),
                    &[&sid, &wid, &rt, &contracts, &node],
                )?;
                Ok(())
            })
            .map_err(storage)
    }

    /// A session opened on another node, if it still exists.
    #[allow(clippy::type_complexity)]
    pub(crate) fn load_session(
        &self,
        session_id: &str,
        session_timeout_ms: u64,
    ) -> Result<Option<(String, String, Vec<(String, String)>)>, EngineError> {
        let sid = session_id.to_string();
        let retain = (session_timeout_ms * 4) as i64;
        let row = self
            .ops()
            .run(move |c| {
                Ok(c.query_opt(
                    &format!(
                        "SELECT worker_id, runtime, contracts FROM orbit_sessions WHERE session_id = $1 AND {NOW_MS} - last_seen_ms < $2"
                    ),
                    &[&sid, &retain],
                )?
                .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1), r.get::<_, String>(2))))
            })
            .map_err(storage)?;
        Ok(row.map(|(w, rt, contracts)| {
            let list: Vec<ContractAnnouncement> =
                serde_json::from_str(&contracts).unwrap_or_default();
            (
                w,
                rt,
                list.into_iter()
                    .map(|c| (c.task_name, c.contract_version))
                    .collect(),
            )
        }))
    }

    /// Delete a session; returns (execution, token) of the tasks it held.
    pub(crate) fn drop_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, String)>, EngineError> {
        let sid = session_id.to_string();
        lock(&self.touched).remove(session_id);
        self.ops()
            .run(move |c| {
                let mut tx = c.transaction()?;
                tx.execute("DELETE FROM orbit_sessions WHERE session_id = $1", &[&sid])?;
                let held = tx
                    .query(
                        "SELECT execution_id, token FROM orbit_tasks WHERE claimed_session = $1 AND state = 'claimed'",
                        &[&sid],
                    )?
                    .into_iter()
                    .map(|r| (r.get(0), r.get(1)))
                    .collect();
                tx.commit()?;
                Ok(held)
            })
            .map_err(storage)
    }

    /// Tokens held by `session` that their owners no longer need (each is
    /// reported once, like the single-server heartbeat).
    pub(crate) fn cancelled_for(&self, session: &str) -> Result<Vec<String>, EngineError> {
        let sid = session.to_string();
        self.ops()
            .run(move |c| {
                Ok(c.query(
                    "UPDATE orbit_tasks SET state = 'cancel_sent', updated_ms = $2
                     WHERE claimed_session = $1 AND state = 'cancelled' RETURNING token",
                    &[&sid, &now_ms()],
                )?
                .into_iter()
                .filter_map(|r| r.get::<_, Option<String>>(0))
                .collect())
            })
            .map_err(storage)
    }

    pub(crate) fn touched(&self, session: &str) {
        lock(&self.touched).insert(session.to_string(), Instant::now());
    }

    /// Whether a worker session is alive, as last read from the database.
    /// Sessions not read yet count as alive.
    pub(crate) fn session_alive(&self, session: &str, timeout_ms: u64) -> bool {
        match lock(&self.seen).get(session) {
            None => true,
            Some(None) => false,
            Some(Some((age, at))) => {
                age + (at.elapsed().as_millis() as u64) < timeout_ms + SESSION_LAG_MS
            }
        }
    }

    /// Whether the attempt holding `token` was handed out by a server that
    /// is gone: the worker may never have received it.
    pub(crate) fn claim_lost(&self, token: Option<&str>) -> bool {
        token.is_some_and(|t| lock(&self.lost).contains(t))
    }

    /// The worker a token issued by another node was given to.
    pub(crate) fn token_worker(&self, token: &str) -> Option<String> {
        let t = token.to_string();
        self.ops()
            .run(move |c| {
                Ok(c.query_opt(
                    "SELECT claimed_worker FROM orbit_tasks WHERE token = $1",
                    &[&t],
                )?
                .and_then(|r| r.get::<_, Option<String>>(0)))
            })
            .ok()
            .flatten()
    }

    /// The worker list with sessions and in-flight tasks across all nodes.
    pub(crate) fn workers(
        &self,
        records: Vec<WorkerRecord>,
        session_timeout_ms: u64,
    ) -> Result<Vec<WorkerInfo>, EngineError> {
        let timeout = session_timeout_ms as i64;
        let (sessions, inflight): (HashMap<String, i64>, HashMap<String, i64>) = self
            .ops()
            .run(move |c| {
                let s = c
                    .query(
                        &format!(
                            "SELECT worker_id, count(*) FROM orbit_sessions WHERE {NOW_MS} - last_seen_ms < $1 GROUP BY worker_id"
                        ),
                        &[&timeout],
                    )?
                    .into_iter()
                    .map(|r| (r.get(0), r.get(1)))
                    .collect();
                let i = c
                    .query(
                        "SELECT claimed_worker, count(*) FROM orbit_tasks WHERE state = 'claimed' AND claimed_worker IS NOT NULL GROUP BY claimed_worker",
                        &[],
                    )?
                    .into_iter()
                    .map(|r| (r.get(0), r.get(1)))
                    .collect();
                Ok((s, i))
            })
            .map_err(storage)?;
        Ok(records
            .into_iter()
            .map(|record| {
                let n = sessions.get(&record.worker_id).copied().unwrap_or(0) as usize;
                WorkerInfo {
                    online: n > 0,
                    sessions: n,
                    inflight: inflight.get(&record.worker_id).copied().unwrap_or(0) as usize,
                    record,
                }
            })
            .collect())
    }

    /// After a takeover: put the adopted executions' queued tasks on the
    /// queue (again), and requeue tasks the previous owner had claimed for
    /// itself without recording the dispatch before it died.
    pub(crate) fn republish(
        &self,
        offers: Vec<(String, TaskOffer)>,
        previous_owner: &str,
    ) -> Result<(), EngineError> {
        if offers.is_empty() {
            return Ok(());
        }
        let previous = previous_owner.to_string();
        self.ops()
            .run(move |c| {
                let mut tx = c.transaction()?;
                let now = now_ms();
                let ins = tx.prepare(
                    "INSERT INTO orbit_tasks (execution_id, unit_key, attempt, offer, task_name, contract_version, language, payload, enqueued_ms, state, updated_ms)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'queued', $10)
                     ON CONFLICT (execution_id, unit_key, attempt, offer) DO NOTHING",
                )?;
                for (id, o) in &offers {
                    tx.execute(
                        &ins,
                        &[id, &o.unit_key, &o.attempt, &o.offer, &o.task_name, &o.contract_version, &o.language, &o.payload, &o.enqueued_ms, &now],
                    )?;
                }
                let units: Vec<(String, String, i64, i64)> = offers
                    .iter()
                    .map(|(id, o)| (id.clone(), o.unit_key.clone(), o.attempt, o.offer))
                    .collect();
                requeue(&mut tx, &units, Some(&previous), None)?;
                tx.execute("SELECT pg_notify('orbit_tasks', '')", &[])?;
                tx.commit()?;
                Ok(())
            })
            .map_err(storage)
    }
}

/// Put claimed rows of still-queued units back on the queue when the claim
/// never reached the owner (claimed by `by` for itself, or older than
/// `older_than`), unless a dispatch for them waits in the inbox.
fn requeue(
    c: &mut impl postgres::GenericClient,
    units: &[(String, String, i64, i64)],
    by: Option<&str>,
    older_than: Option<i64>,
) -> Result<u64, postgres::Error> {
    if units.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = units.iter().map(|u| u.0.clone()).collect();
    let keys: Vec<String> = units.iter().map(|u| u.1.clone()).collect();
    let attempts: Vec<i64> = units.iter().map(|u| u.2).collect();
    let offers: Vec<i64> = units.iter().map(|u| u.3).collect();
    c.execute(
        "UPDATE orbit_tasks t SET state = 'queued', token = NULL, claimed_by = NULL, claimed_inc = NULL,
             claimed_session = NULL, claimed_worker = NULL, claimed_ms = NULL, updated_ms = $7
         WHERE t.state = 'claimed'
           AND ($5::text IS NULL OR t.claimed_by = $5)
           AND ($6::bigint IS NULL OR t.claimed_ms < $6)
           AND (t.execution_id, t.unit_key, t.attempt, t.offer) IN
               (SELECT * FROM unnest($1::text[], $2::text[], $3::bigint[], $4::bigint[]))
           AND NOT EXISTS (SELECT 1 FROM orbit_inbox i WHERE i.execution_id = t.execution_id AND i.kind = 'dispatched' AND i.token = t.token)",
        &[&ids, &keys, &attempts, &offers, &by, &older_than, &now_ms()],
    )
}

// ------------------------------------------------------------------ setup

fn connect(config: &SharedConfig, purpose: &str) -> Result<postgres::Client, String> {
    let mut c = session_client(
        &config.url,
        &format!("orbit-shared-{purpose}:{}", config.node_id),
    )
    .map_err(|e| e.to_string())?;
    c.batch_execute("SET statement_timeout = '30s'")
        .map_err(|e| e.to_string())?;
    let mut tx = c.transaction().map_err(|e| e.to_string())?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&SCHEMA_LOCK_KEY])
        .map_err(|e| e.to_string())?;
    let exists: bool = tx
        .query_one("SELECT to_regclass('orbit_sessions') IS NOT NULL", &[])
        .map_err(|e| e.to_string())?
        .get(0);
    if !exists {
        tx.batch_execute(SHARED_SCHEMA).map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(c)
}

/// Register this node in `orbit_nodes`. A previous process with the same
/// node id that is still heartbeating is waited for (a quick restart) for up
/// to three node timeouts; after that, starting is refused.
fn register_node(
    c: &mut postgres::Client,
    config: &SharedConfig,
    incarnation: &str,
) -> Result<(), String> {
    let timeout = config.node_timeout.as_millis() as i64;
    let deadline = Instant::now() + config.node_timeout * 3;
    let mut warned = false;
    loop {
        let row = c
            .query_opt(
                &format!(
                    "SELECT incarnation, {NOW_MS} - heartbeat_ms FROM orbit_nodes WHERE node_id = $1"
                ),
                &[&config.node_id],
            )
            .map_err(|e| e.to_string())?;
        if let Some((inc, age)) = row.map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1))) {
            if inc != incarnation && age < timeout {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "another server with node id `{}` is running (it heartbeated {age} ms ago); give every server its own --node-id",
                        config.node_id
                    ));
                }
                if !warned {
                    tracing::warn!(node_id = %config.node_id, "shared: a server with this node id heartbeated recently; waiting for it to expire");
                    warned = true;
                }
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
        }
        c.execute(
            &format!(
                "INSERT INTO orbit_nodes (node_id, incarnation, addr, heartbeat_ms, started_ms) VALUES ($1, $2, $3, {NOW_MS}, {NOW_MS})
                 ON CONFLICT (node_id) DO UPDATE SET incarnation = EXCLUDED.incarnation, addr = EXCLUDED.addr,
                     heartbeat_ms = EXCLUDED.heartbeat_ms, started_ms = EXCLUDED.started_ms"
            ),
            &[&config.node_id, &incarnation, &config.advertise_addr],
        )
        .map_err(|e| e.to_string())?;
        return Ok(());
    }
}

/// Open a shared-mode engine: builds it (through `factory`) on a new
/// shared-mode store and [`attach`]es it.
pub fn open(
    config: SharedConfig,
    factory: impl Fn(Store) -> Result<Engine, EngineError>,
) -> Result<Engine, EngineError> {
    let store = PgStore::connect_with(&config.url, PgMode::Shared).map_err(storage)?;
    attach(factory(Store::Postgres(store))?, config)
}

/// Turn an engine opened on a [`PgMode::Shared`] store into a shared-mode
/// node: creates the shared tables and registers the node. Serve it with
/// [`crate::server::serve_node`], which starts the node's threads; the node
/// then resumes or takes over executions.
pub fn attach(mut engine: Engine, config: SharedConfig) -> Result<Engine, EngineError> {
    let unavailable = |e: String| EngineError::new(500, "STORAGE_ERROR", e);
    let incarnation = uuid::Uuid::new_v4().to_string();
    let mut c = connect(&config, "control").map_err(unavailable)?;
    register_node(&mut c, &config, &incarnation).map_err(unavailable)?;
    drop(c);
    let pool = (0..OPS_CONNECTIONS)
        .map(|_| PgStore::connect_with(&config.url, PgMode::Shared))
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?;
    let ctx = Arc::new(Ctx {
        session_timeout_ms: engine.config.session_timeout_ms,
        config,
        incarnation,
        pool,
        next: std::sync::atomic::AtomicUsize::new(0),
        epochs: Arc::new(Mutex::new(HashMap::new())),
        staged: Mutex::new(vec![]),
        done: Mutex::new(vec![]),
        fenced: Mutex::new(HashSet::new()),
        touched: Mutex::new(HashMap::new()),
        seen: Mutex::new(HashMap::new()),
        lost: Mutex::new(HashSet::new()),
        queue_gen: AtomicU64::new(0),
        empty: Mutex::new(HashMap::new()),
        defs_dirty: AtomicBool::new(false),
        applying: AtomicBool::new(false),
        started: AtomicBool::new(false),
        duplicate: AtomicBool::new(false),
        last_beat: Mutex::new(Some(Instant::now())),
        nodes: Mutex::new(json!([])),
        tasks_claimed: AtomicU64::new(0),
        stop: AtomicBool::new(false),
        halt: AtomicBool::new(false),
        spawned: AtomicBool::new(false),
        exited: AtomicBool::new(false),
    });
    engine.shared = Some(ctx);
    Ok(engine)
}

/// Start the node's control and notification threads. Called by
/// [`crate::server::serve_node`] for a shared-mode engine.
pub fn start(ctx: Arc<Ctx>, state: Shared) -> std::io::Result<()> {
    tracing::info!(
        node_id = %ctx.config.node_id,
        advertise_addr = ctx.config.advertise_addr.as_deref().unwrap_or(""),
        node_timeout_ms = ctx.config.node_timeout.as_millis() as u64,
        "cluster mode: shared (every server active)"
    );
    let (wake_tx, wake_rx) = mpsc::channel();
    let (c, s) = (ctx.clone(), state.clone());
    std::thread::Builder::new()
        .name("orbit-shared".into())
        .spawn(move || Control::new(c, s).run(wake_rx))?;
    ctx.spawned.store(true, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("orbit-shared-listen".into())
        .spawn(move || listen_loop(&ctx, &state, wake_tx))?;
    Ok(())
}

// --------------------------------------------------------------- listener

/// LISTEN for queue, inbox, completion and definition notifications.
fn listen_loop(ctx: &Ctx, state: &Shared, wake: mpsc::Sender<()>) {
    while !ctx.stopped() {
        let mut c = match connect(&ctx.config, "listen") {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "shared: cannot open the notification session; retrying");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        if let Err(e) = c.batch_execute(
            "LISTEN orbit_inbox; LISTEN orbit_tasks; LISTEN orbit_done; LISTEN orbit_defs",
        ) {
            tracing::warn!(error = %e, "shared: LISTEN failed; retrying");
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        // Anything may have happened while not listening.
        ctx.queue_gen.fetch_add(1, Ordering::SeqCst);
        ctx.defs_dirty.store(true, Ordering::SeqCst);
        let _ = wake.send(());
        state.changed.notify_waiters();
        'session: while !ctx.stopped() {
            {
                let mut n = c.notifications();
                let mut it = n.timeout_iter(Duration::from_millis(500));
                loop {
                    match it.next() {
                        Ok(None) => break,
                        Ok(Some(note)) => match note.channel() {
                            "orbit_inbox" => {
                                if note.payload() == ctx.config.node_id {
                                    let _ = wake.send(());
                                }
                            }
                            "orbit_tasks" => {
                                ctx.queue_gen.fetch_add(1, Ordering::SeqCst);
                                state.changed.notify_waiters();
                            }
                            "orbit_done" => state.changed.notify_waiters(),
                            "orbit_defs" => ctx.defs_dirty.store(true, Ordering::SeqCst),
                            _ => {}
                        },
                        Err(e) => {
                            tracing::warn!(error = %e, "shared: notification session failed; reconnecting");
                            break 'session;
                        }
                    }
                }
            }
            if c.is_closed() {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------- control

struct Control {
    ctx: Arc<Ctx>,
    state: Shared,
    resumed_own: bool,
    beat: Instant,
    flush: Instant,
    seen: Instant,
    notify: Instant,
    reconcile: Instant,
    cleanup: Instant,
}

impl Control {
    fn new(ctx: Arc<Ctx>, state: Shared) -> Control {
        let past = Instant::now() - Duration::from_secs(3600);
        Control {
            ctx,
            state,
            resumed_own: false,
            beat: past,
            flush: past,
            seen: past,
            notify: past,
            reconcile: Instant::now(),
            cleanup: Instant::now(),
        }
    }

    fn me(&self) -> &str {
        &self.ctx.config.node_id
    }

    fn run(mut self, wake: mpsc::Receiver<()>) {
        let mut client: Option<postgres::Client> = None;
        while !self.ctx.stopped() {
            if self.ctx.halted() {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            if client.as_ref().is_none_or(|c| c.is_closed()) {
                match connect(&self.ctx.config, "control") {
                    Ok(c) => client = Some(c),
                    Err(e) => {
                        tracing::warn!(error = %e, "shared: cannot reach PostgreSQL; retrying");
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                }
            }
            let c = client.as_mut().expect("connected");
            if let Err(e) = self.cycle(c) {
                tracing::warn!(error = %e, "shared: control cycle failed");
                std::thread::sleep(Duration::from_millis(200));
            }
            let _ = wake.recv_timeout(Duration::from_millis(100));
            while wake.try_recv().is_ok() {}
        }
        if !self.ctx.halted() {
            if let Some(c) = client.as_mut() {
                // Tasks this server handed out were delivered: they live on
                // with their worker sessions, not with this server.
                let _ = c.execute(
                    "UPDATE orbit_tasks SET claimed_inc = NULL WHERE claimed_inc = $1",
                    &[&self.ctx.incarnation],
                );
                let _ = c.execute(
                    "UPDATE orbit_nodes SET heartbeat_ms = 0 WHERE node_id = $1 AND incarnation = $2",
                    &[&self.ctx.config.node_id, &self.ctx.incarnation],
                );
                tracing::info!(node_id = %self.ctx.config.node_id, "shared: left the cluster; other servers take over this server's executions");
            }
        }
        self.ctx.exited.store(true, Ordering::SeqCst);
    }

    fn cycle(&mut self, c: &mut postgres::Client) -> Result<(), String> {
        if !self.resumed_own {
            // Executions this node id owned before a restart: fence the
            // previous process (epoch + 1) and resume them.
            let rows = c
                .query(
                    &format!(
                        "UPDATE orbit_owners SET epoch = epoch + 1, since_ms = {NOW_MS} WHERE node_id = $1 RETURNING execution_id, epoch"
                    ),
                    &[&self.ctx.config.node_id],
                )
                .map_err(|e| e.to_string())?;
            let items: Vec<(String, i64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
            if !items.is_empty() {
                tracing::info!(
                    executions = items.len(),
                    "shared: resuming the executions this node owned before it restarted"
                );
                let me = self.me().to_string();
                engine(&self.state)
                    .adopt(&items, &me)
                    .map_err(|e| e.to_string())?;
            }
            self.resumed_own = true;
            *lock(&self.ctx.last_beat) = Some(Instant::now());
            self.ctx.started.store(true, Ordering::SeqCst);
            tracing::info!(node_id = %self.me(), resumed = items.len(), "shared: node is serving");
        }
        if self.beat.elapsed() >= self.ctx.config.heartbeat_interval {
            self.beat = Instant::now();
            self.heartbeat(c)?;
            self.take_over(c)?;
            self.evict_fenced();
        }
        self.consume_inbox(c)?;
        if self.flush.elapsed() >= Duration::from_millis(250) {
            self.flush = Instant::now();
            self.flush_touches(c)?;
        }
        if self.seen.elapsed() >= Duration::from_millis(500) {
            self.seen = Instant::now();
            self.refresh_seen(c)?;
        }
        if self.notify.elapsed() >= Duration::from_secs(1) {
            self.notify = Instant::now();
            if self.ctx.defs_dirty.swap(false, Ordering::SeqCst) {
                let _ = engine(&self.state).refresh_definitions();
            }
            // Fallback for missed notifications: waits and polls look again.
            self.state.changed.notify_waiters();
        }
        if self.reconcile.elapsed() >= Duration::from_secs(10) {
            self.reconcile = Instant::now();
            self.reconcile_ownership(c)?;
            self.sweep_claims(c)?;
        }
        if self.cleanup.elapsed() >= Duration::from_secs(30) {
            self.cleanup = Instant::now();
            self.cleanup(c)?;
        }
        Ok(())
    }

    fn heartbeat(&mut self, c: &mut postgres::Client) -> Result<(), String> {
        let ctx = self.ctx.clone();
        let cfg = &ctx.config;
        let n = c
            .execute(
                &format!(
                    "UPDATE orbit_nodes SET heartbeat_ms = {NOW_MS}, addr = $3 WHERE node_id = $1 AND incarnation = $2"
                ),
                &[&cfg.node_id, &ctx.incarnation, &cfg.advertise_addr],
            )
            .map_err(|e| e.to_string())?;
        let previous = *lock(&ctx.last_beat);
        if n == 0 {
            let row = c
                .query_opt(
                    &format!("SELECT incarnation, {NOW_MS} - heartbeat_ms FROM orbit_nodes WHERE node_id = $1"),
                    &[&cfg.node_id],
                )
                .map_err(|e| e.to_string())?
                .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1)));
            if let Some((_, age)) = row.filter(|(inc, age)| {
                *inc != ctx.incarnation && *age < cfg.node_timeout.as_millis() as i64
            }) {
                if !ctx.duplicate.swap(true, Ordering::SeqCst) {
                    tracing::error!(node_id = %cfg.node_id, age_ms = age, "shared: another server took this node id; this server stops serving until it is gone");
                }
                let mut e = engine(&self.state);
                for id in e.owned_ids() {
                    e.evict(&id);
                }
                return Ok(());
            }
            register_node(c, cfg, &ctx.incarnation)?;
        }
        ctx.duplicate.store(false, Ordering::SeqCst);
        *lock(&ctx.last_beat) = Some(Instant::now());
        if n == 0 || previous.is_none_or(|p| p.elapsed() >= cfg.node_timeout) {
            // Other nodes may have taken over while this one was not heartbeating.
            self.reconcile_ownership(c)?;
        }
        let rows = c
            .query(
                &format!(
                    "SELECT n.node_id, n.addr, {NOW_MS} - n.heartbeat_ms,
                            (SELECT count(*) FROM orbit_owners o WHERE o.node_id = n.node_id)
                     FROM orbit_nodes n ORDER BY n.node_id"
                ),
                &[],
            )
            .map_err(|e| e.to_string())?;
        let timeout = cfg.node_timeout.as_millis() as i64;
        let nodes: Vec<Value> = rows
            .iter()
            .map(|r| {
                let age: i64 = r.get(2);
                json!({
                    "node_id": r.get::<_, String>(0),
                    "addr": r.get::<_, Option<String>>(1),
                    "live": age < timeout,
                    "heartbeat_age_ms": age.max(0),
                    "owned_executions": r.get::<_, i64>(3),
                })
            })
            .filter(|n| n["live"] == json!(true) || n["owned_executions"] != json!(0))
            .collect();
        *lock(&ctx.nodes) = Value::Array(nodes);
        Ok(())
    }

    /// Take over the executions of dead nodes: this node's share, chosen by
    /// rendezvous hashing over the live nodes, one execution tree at a time.
    fn take_over(&mut self, c: &mut postgres::Client) -> Result<(), String> {
        let timeout = self.ctx.config.node_timeout.as_millis() as i64;
        let batch = self.ctx.config.takeover_batch.max(1);
        let me = self.me().to_string();
        loop {
            let picks = c
                .query(
                    &format!(
                        "WITH live AS (SELECT node_id FROM orbit_nodes WHERE {NOW_MS} - heartbeat_ms < $1),
                         orphan AS (
                             SELECT DISTINCT ON (o.root_id) o.root_id, o.node_id AS old
                             FROM orbit_owners o
                             WHERE NOT EXISTS (SELECT 1 FROM live l WHERE l.node_id = o.node_id)
                             ORDER BY o.root_id),
                         pick AS (
                             SELECT DISTINCT ON (orphan.root_id) orphan.root_id, orphan.old, l.node_id AS taker
                             FROM orphan CROSS JOIN live l
                             ORDER BY orphan.root_id, md5(orphan.root_id || '/' || l.node_id) DESC)
                         SELECT root_id, old FROM pick WHERE taker = $2 LIMIT $3"
                    ),
                    &[&timeout, &me, &batch],
                )
                .map_err(|e| e.to_string())?;
            if picks.is_empty() {
                return Ok(());
            }
            let mut by_old: BTreeMap<String, Vec<(String, i64)>> = BTreeMap::new();
            for p in &picks {
                let (root, old): (String, String) = (p.get(0), p.get(1));
                let won = take_over_root(c, &me, &root, &old).map_err(|e| e.to_string())?;
                by_old.entry(old).or_default().extend(won);
            }
            for (old, items) in by_old {
                if items.is_empty() {
                    continue;
                }
                tracing::warn!(from = %old, executions = items.len(), "shared: taking over executions of a node that stopped heartbeating");
                engine(&self.state)
                    .adopt(&items, &old)
                    .map_err(|e| e.to_string())?;
            }
            if (picks.len() as i64) < batch {
                return Ok(());
            }
        }
    }

    fn evict_fenced(&mut self) {
        let fenced: Vec<String> = lock(&self.ctx.fenced).drain().collect();
        if fenced.is_empty() {
            return;
        }
        let mut e = engine(&self.state);
        for id in fenced {
            e.evict(&id);
        }
    }

    /// Compare what this node holds with `orbit_owners`: drop executions
    /// another node took over, load executions recorded as this node's.
    fn reconcile_ownership(&mut self, c: &mut postgres::Client) -> Result<(), String> {
        let mut e = engine(&self.state);
        let rows = c
            .query(
                "SELECT execution_id, epoch FROM orbit_owners WHERE node_id = $1",
                &[&self.ctx.config.node_id],
            )
            .map_err(|e| e.to_string())?;
        let db: HashMap<String, i64> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
        let held = lock(&self.ctx.epochs).clone();
        for id in e.owned_ids() {
            if db.get(&id) != held.get(&id) {
                e.evict(&id);
            }
        }
        let missing: Vec<(String, i64)> = db.into_iter().filter(|(id, _)| !e.owns(id)).collect();
        if !missing.is_empty() {
            let me = self.me().to_string();
            e.adopt(&missing, &me).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Requeue claims older than 30 s that never reached this owner (a
    /// safety net; claims normally reach the owner atomically).
    fn sweep_claims(&mut self, c: &mut postgres::Client) -> Result<(), String> {
        let units = engine(&self.state).queued_units();
        let n = requeue(c, &units, None, Some(now_ms() - 30_000)).map_err(|e| e.to_string())?;
        if n > 0 {
            tracing::warn!(
                tasks = n,
                "shared: requeued tasks whose claim never reached this owner"
            );
            let _ = c.execute("SELECT pg_notify('orbit_tasks', '')", &[]);
        }
        Ok(())
    }

    /// Apply inbox entries for executions this node owns, in order.
    fn consume_inbox(&mut self, c: &mut postgres::Client) -> Result<(), String> {
        let rows = c
            .query(
                "SELECT i.seq, i.execution_id, i.kind, i.payload FROM orbit_inbox i
                 JOIN orbit_owners o ON o.execution_id = i.execution_id
                 WHERE o.node_id = $1 AND i.done_ms IS NULL
                 ORDER BY i.seq LIMIT 500",
                &[&self.ctx.config.node_id],
            )
            .map_err(|e| e.to_string())?;
        let mut blocked: HashSet<String> = HashSet::new();
        for row in rows {
            let (seq, exec, kind, payload): (i64, String, String, String) =
                (row.get(0), row.get(1), row.get(2), row.get(3));
            if blocked.contains(&exec) {
                continue;
            }
            let p: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            let mut e = engine(&self.state);
            if !e.owns(&exec) {
                blocked.insert(exec);
                continue;
            }
            self.ctx.stage(seq, &exec, &kind);
            self.ctx.applying.store(true, Ordering::SeqCst);
            let r = apply_entry(&mut e, &exec, &kind, &p);
            self.ctx.applying.store(false, Ordering::SeqCst);
            drop(e);
            if self.ctx.unstage(seq) {
                continue;
            }
            let reply = |v: String| matches!(kind.as_str(), "signal" | "cancel").then_some(v);
            let finish = match r {
                Err(err) if err.code == "STORAGE_ERROR" => {
                    // Fenced or the database failed: leave it for the owner.
                    tracing::warn!(execution_id = %exec, kind = %kind, error = %err, "shared: applying an inbox entry failed");
                    blocked.insert(exec);
                    continue;
                }
                Err(err) => reply(error_json(err.status, err.code, &err.message)),
                Ok(()) => reply(json!({ "ok": true }).to_string()),
            };
            match finish {
                None => c.execute("DELETE FROM orbit_inbox WHERE seq = $1", &[&seq]),
                Some(r) => c.execute(
                    "UPDATE orbit_inbox SET done_ms = $2, result = $3 WHERE seq = $1",
                    &[&seq, &now_ms(), &r],
                ),
            }
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Write the last contact of sessions this node served to the database.
    fn flush_touches(&mut self, c: &mut postgres::Client) -> Result<(), String> {
        let touched: Vec<(String, Instant)> = lock(&self.ctx.touched).drain().collect();
        if touched.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = touched.iter().map(|t| t.0.clone()).collect();
        let ago: Vec<i64> = touched
            .iter()
            .map(|t| t.1.elapsed().as_millis() as i64)
            .collect();
        let found: HashSet<String> = c
            .query(
                &format!(
                    "UPDATE orbit_sessions s SET last_seen_ms = GREATEST(s.last_seen_ms, {NOW_MS} - v.ago)
                     FROM unnest($1::text[], $2::bigint[]) AS v(id, ago)
                     WHERE s.session_id = v.id RETURNING s.session_id"
                ),
                &[&ids, &ago],
            )
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|r| r.get(0))
            .collect();
        let gone: Vec<String> = ids.into_iter().filter(|i| !found.contains(i)).collect();
        if !gone.is_empty() {
            engine(&self.state).forget_sessions(&gone);
        }
        Ok(())
    }

    /// Read the last contact of the sessions holding this node's attempts,
    /// and which of those attempts were handed out by a server that is gone.
    fn refresh_seen(&mut self, c: &mut postgres::Client) -> Result<(), String> {
        let attempts = engine(&self.state).dispatched_attempts();
        let mut sessions: Vec<String> = attempts.iter().map(|a| a.0.clone()).collect();
        sessions.sort();
        sessions.dedup();
        let tokens: Vec<String> = attempts.into_iter().map(|a| a.1).collect();
        let mut lost = HashSet::new();
        if !tokens.is_empty() {
            let session_timeout = self.ctx.session_timeout_ms as i64;
            let grace = self.ctx.config.node_timeout.as_millis() as i64 + session_timeout;
            for r in c
                .query(
                    &format!(
                        "SELECT t.token FROM orbit_tasks t LEFT JOIN orbit_nodes n ON n.node_id = t.claimed_by
                         WHERE t.token = ANY($1) AND t.state <> 'done' AND t.claimed_inc IS NOT NULL
                           AND (n.node_id IS NULL
                                OR {NOW_MS} - n.heartbeat_ms > $2
                                OR (n.incarnation <> t.claimed_inc AND {NOW_MS} - n.started_ms > $3))"
                    ),
                    &[&tokens, &grace, &session_timeout],
                )
                .map_err(|e| e.to_string())?
            {
                lost.insert(r.get::<_, String>(0));
            }
        }
        *lock(&self.ctx.lost) = lost;
        let mut seen: HashMap<String, Option<(u64, Instant)>> =
            sessions.iter().map(|s| (s.clone(), None)).collect();
        if !sessions.is_empty() {
            let at = Instant::now();
            for r in c
                .query(
                    &format!(
                        "SELECT session_id, {NOW_MS} - last_seen_ms FROM orbit_sessions WHERE session_id = ANY($1)"
                    ),
                    &[&sessions],
                )
                .map_err(|e| e.to_string())?
            {
                let age: i64 = r.get(1);
                seen.insert(r.get(0), Some((age.max(0) as u64, at)));
            }
        }
        *lock(&self.ctx.seen) = seen;
        Ok(())
    }

    fn cleanup(&mut self, c: &mut postgres::Client) -> Result<(), String> {
        let retain = (self.ctx.session_timeout_ms * 4).max(60_000) as i64;
        let cutoff = now_ms() - retain;
        let e = |e: postgres::Error| e.to_string();
        c.execute(
            &format!("DELETE FROM orbit_sessions WHERE {NOW_MS} - last_seen_ms > $1"),
            &[&((self.ctx.session_timeout_ms * 4) as i64)],
        )
        .map_err(e)?;
        c.execute(
            "DELETE FROM orbit_tasks t WHERE t.state NOT IN ('queued', 'claimed') AND t.updated_ms < $1
             AND NOT EXISTS (SELECT 1 FROM orbit_owners o WHERE o.execution_id = t.execution_id)",
            &[&cutoff],
        )
        .map_err(e)?;
        c.execute(
            "DELETE FROM orbit_inbox i WHERE i.created_ms < $1
             AND (i.done_ms IS NOT NULL OR NOT EXISTS (SELECT 1 FROM orbit_owners o WHERE o.execution_id = i.execution_id))",
            &[&(now_ms() - 60_000)],
        )
        .map_err(e)?;
        c.execute(
            &format!(
                "DELETE FROM orbit_nodes n WHERE {NOW_MS} - n.heartbeat_ms > 3600000
                 AND NOT EXISTS (SELECT 1 FROM orbit_owners o WHERE o.node_id = n.node_id)"
            ),
            &[],
        )
        .map_err(e)?;
        Ok(())
    }
}

/// Take over one execution tree from `old`: every execution of `root` that
/// `old` owns moves to `me` with its epoch incremented. Concurrent callers
/// each win a disjoint set of rows (the update re-checks `node_id = old`
/// after waiting for a competing update). Returns (execution, new epoch).
pub fn take_over_root(
    c: &mut postgres::Client,
    me: &str,
    root: &str,
    old: &str,
) -> Result<Vec<(String, i64)>, postgres::Error> {
    Ok(c.query(
        &format!(
            "UPDATE orbit_owners SET node_id = $1, epoch = epoch + 1, since_ms = {NOW_MS}
                 WHERE root_id = $2 AND node_id = $3 RETURNING execution_id, epoch"
        ),
        &[&me, &root, &old],
    )?
    .iter()
    .map(|r| (r.get(0), r.get(1)))
    .collect())
}

/// `POST /v1/workers/poll` (HTTP and gRPC): [`Engine::poll`], except that in
/// shared mode the claim on the queue runs without holding the engine, so
/// that polls on one server do not wait for each other's database round trips.
pub(crate) fn poll(state: &Shared, session: &str) -> Result<Option<TaskAssignment>, EngineError> {
    let (ctx, worker, runtime, contracts) = {
        let mut e = engine(state);
        let Some(ctx) = e.shared_context() else {
            return e.poll(session);
        };
        match e.prepare_claim(session)? {
            Some((w, r, c)) => (ctx, w, r, c),
            None => return Ok(None),
        }
    };
    for _ in 0..16 {
        let now = engine(state).now();
        match ctx.claim(session, &worker, &runtime, &contracts, now)? {
            Claimed::Nothing => return Ok(None),
            Claimed::Stale => continue,
            Claimed::Remote(a) => return Ok(Some(a)),
            Claimed::Local {
                row_id,
                offer,
                assignment: a,
            } => {
                let applied = engine(state).apply_dispatch(
                    &a.execution_id,
                    &a.step_id,
                    a.attempt,
                    offer,
                    &a.token,
                    &worker,
                    session,
                    a.deadline_ms,
                );
                match applied {
                    Ok(true) => return Ok(Some(a)),
                    Ok(false) => ctx.retire(row_id, &a.token)?,
                    Err(e) => {
                        let _ = ctx.release(row_id, &a.token);
                        return Err(e);
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Apply one inbox entry through the engine.
fn apply_entry(e: &mut Engine, exec: &str, kind: &str, p: &Value) -> Result<(), EngineError> {
    let s = |k: &str| p[k].as_str().unwrap_or("").to_string();
    match kind {
        "dispatched" => {
            e.apply_dispatch(
                exec,
                &s("unit"),
                p["attempt"].as_u64().unwrap_or(0) as u32,
                p["offer"].as_u64().unwrap_or(0) as u32,
                &s("token"),
                &s("worker_id"),
                &s("session"),
                p["deadline_ms"].as_u64(),
            )?;
        }
        "complete" => {
            e.complete(&s("token"), p["output"].clone())?;
        }
        "fail" => {
            let error = serde_json::from_value(p["error"].clone()).unwrap_or_else(|_| {
                crate::model::TaskError::new("UNKNOWN", "unreadable failure", true)
            });
            e.fail(&s("token"), error)?;
        }
        "uncertain" => {
            e.mark_uncertain(&s("token"), &s("reason"))?;
        }
        "signal" => {
            e.signal(exec, &s("name"), p["payload"].clone())?;
        }
        "cancel" => {
            e.cancel(exec, &s("reason"))?;
        }
        other => tracing::warn!(kind = other, "shared: unknown inbox entry"),
    }
    Ok(())
}

// --------------------------------------------------------------- HTTP gate

#[derive(Clone)]
struct Gate {
    ctx: Arc<Ctx>,
    state: Shared,
}

/// Wrap the API router for shared mode: `/v1/ready`, the shared fields of
/// `/v1/health`, fresh definitions, and replies for signals and cancels that
/// were forwarded to another node.
pub fn layer(app: Router, ctx: Arc<Ctx>, state: Shared) -> Router {
    app.layer(middleware::from_fn_with_state(Gate { ctx, state }, gate))
}

fn health_fields(ctx: &Ctx) -> Value {
    json!({
        "role": "shared",
        "node_id": ctx.config.node_id,
        "term": null,
        "leader": null,
        "owned_executions": ctx.owned_executions(),
        "tasks_claimed": ctx.tasks_claimed(),
        "nodes": lock(&ctx.nodes).clone(),
    })
}

async fn gate(State(g): State<Gate>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if path == "/v1/ready" {
        let ready = g.ctx.is_ready();
        let mut body = health_fields(&g.ctx);
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
        return crate::cluster::merge_json(resp, health_fields(&g.ctx)).await;
    }
    if g.ctx.defs_dirty.swap(false, Ordering::SeqCst) {
        let st = g.state.clone();
        let _ = tokio::task::spawn_blocking(move || engine(&st).refresh_definitions()).await;
    }
    let post = req.method() == Method::POST;
    let forwarding = post
        && path.starts_with("/v1/executions/")
        && (path.ends_with("/cancel") || path.contains("/signals/"));
    if forwarding {
        let slot = Arc::new(Mutex::new(None));
        let resp = FORWARDED.scope(slot.clone(), next.run(req)).await;
        let seq = *lock(&slot);
        return match seq {
            Some(seq) if resp.status() == StatusCode::OK => {
                finish_forwarded(&g, &path, seq, resp).await
            }
            _ => resp,
        };
    }
    let registers = post && path == "/v1/workflows";
    let resp = next.run(req).await;
    if registers && resp.status() == StatusCode::CREATED {
        let ctx = g.ctx.clone();
        let _ = tokio::task::spawn_blocking(move || {
            ctx.ops().run(|c| {
                c.execute("SELECT pg_notify('orbit_defs', '')", &[])?;
                Ok(())
            })
        })
        .await;
    }
    resp
}

/// The owner applied (or not yet) a forwarded signal or cancel: answer as a
/// single server would.
async fn finish_forwarded(g: &Gate, path: &str, seq: i64, resp: Response) -> Response {
    let ctx = g.ctx.clone();
    let reply = tokio::task::spawn_blocking(move || ctx.wait_reply(seq, REPLY_TIMEOUT))
        .await
        .ok()
        .flatten();
    if let Some(err) = reply.as_ref().and_then(|r| r.get("error")) {
        let status = err["status"]
            .as_u64()
            .and_then(|s| StatusCode::from_u16(s as u16).ok())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let (parts, _) = resp.into_parts();
        let mut out = (
            status,
            Json(json!({ "error": { "code": err["code"], "message": err["message"] } })),
        )
            .into_response();
        for (k, v) in parts.headers.iter() {
            if k != "content-length" && k != "content-type" {
                out.headers_mut().insert(k, v.clone());
            }
        }
        return out;
    }
    let id = path
        .trim_start_matches("/v1/executions/")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string();
    if path.ends_with("/cancel") {
        if reply.is_none() {
            return resp;
        }
        let st = g.state.clone();
        return match tokio::task::spawn_blocking(move || engine(&st).execution(&id)).await {
            Ok(Ok(s)) => {
                let (parts, _) = resp.into_parts();
                let body = axum::body::Body::from(serde_json::to_vec(&s).unwrap_or_default());
                let mut out = Response::from_parts(parts, body);
                out.headers_mut().remove("content-length");
                out
            }
            _ => resp,
        };
    }
    let seq_value = reply
        .and_then(|r| r.get("seq").cloned())
        .unwrap_or(Value::Null);
    crate::cluster::merge_json(resp, json!({ "seq": seq_value })).await
}
