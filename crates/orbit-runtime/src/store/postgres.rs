//! PostgreSQL backend.
//!
//! The `postgres` crate drives its own async runtime internally and must not be
//! called from inside another one, while the engine runs synchronously inside
//! the HTTP server's tokio runtime. So every statement runs on one dedicated
//! connection thread, fed through a channel. Calls stay synchronous for the
//! engine, the connection is reconnected automatically if it drops, and each
//! `append` is a single transaction, exactly like the SQLite backend.

use super::{
    ExecutionSummary, FireRecord, PutDefinition, Result, Rollover, ScheduleRow, StoreError,
    WorkerRecord, SCHEMA,
};
use crate::model::{Event, ExecutionState, HistoryEvent};
use orbit_ir::WorkflowIr;
use postgres::Client;
use std::sync::mpsc;
use tokio_postgres_rustls::MakeRustlsConnect;

type Job = Box<dyn FnOnce(&mut Client) + Send>;

pub struct PgStore {
    jobs: mpsc::Sender<Job>,
    mode: PgMode,
}

/// How a [`PgStore`] may write. Single-node servers use `ReadWrite`; the
/// cluster mode (`orbit server --cluster`, see `crate::cluster`) gives a
/// standby a `ReadOnly` store and the leader a `Fenced` one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgMode {
    ReadWrite,
    /// Reads only: writes fail and no execution counts as active, so an
    /// engine opened on it recovers nothing and never appends.
    ReadOnly,
    /// Every history append verifies, in its own transaction, that the
    /// cluster term is still this one, so a deposed leader cannot write.
    Fenced(i64),
    /// Shared mode (`--cluster-mode shared`, see `crate::shared`): every node
    /// writes, but only to the executions it owns. Appends go through
    /// [`PgStore::append_shared`], which checks the execution's ownership
    /// epoch; no execution counts as active at open, because a node loads
    /// the executions it owns or takes over explicitly.
    Shared,
}

/// Table holding the cluster's leadership term (created by `crate::cluster`).
pub(crate) const CLUSTER_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orbit_cluster (
    id INT PRIMARY KEY,
    term BIGINT NOT NULL,
    leader_node TEXT,
    leader_addr TEXT,
    since_ms BIGINT NOT NULL
);
INSERT INTO orbit_cluster (id, term, since_ms) VALUES (1, 0, 0) ON CONFLICT DO NOTHING;
"#;

/// A plain session for the cluster's leader-election connection: same TLS
/// rules as the store, plus timeouts so that a dead network path is noticed
/// within seconds rather than minutes.
pub(crate) fn session_client(url: &str, application_name: &str) -> Result<Client> {
    use std::str::FromStr;
    use std::time::Duration;
    let mut config = postgres::Config::from_str(url)?;
    config
        .application_name(application_name)
        .connect_timeout(Duration::from_secs(5))
        .keepalives(true)
        .keepalives_idle(Duration::from_secs(5))
        .tcp_user_timeout(Duration::from_secs(5));
    Ok(config.connect(tls()?)?)
}

fn read_only() -> StoreError {
    StoreError("this server is a standby: the store is read-only".into())
}

/// TLS for PostgreSQL connections, following the URL's `sslmode`: `prefer`
/// (the default) uses TLS when the server offers it, `require` insists on it,
/// `disable` never uses it. Certificates are verified against the public web
/// PKI roots plus, if set, the PEM CA bundle in `ORBIT_DB_CA_CERT`.
fn tls() -> Result<MakeRustlsConnect> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Ok(path) = std::env::var("ORBIT_DB_CA_CERT") {
        let pem = std::fs::read(&path)
            .map_err(|e| StoreError(format!("cannot read ORBIT_DB_CA_CERT {path}: {e}")))?;
        for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
            let cert =
                cert.map_err(|e| StoreError(format!("invalid certificate in {path}: {e}")))?;
            roots
                .add(cert)
                .map_err(|e| StoreError(format!("invalid CA certificate in {path}: {e}")))?;
        }
    }
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| StoreError(format!("TLS configuration: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

fn connect_client(url: &str) -> Result<Client> {
    let mut client = Client::connect(url, tls()?)?;
    client.batch_execute(SCHEMA)?;
    let rows = client.query_one("SELECT COUNT(*) FROM schema_version", &[])?;
    if rows.get::<_, i64>(0) == 0 {
        client.execute("INSERT INTO schema_version (version) VALUES (1)", &[])?;
    }
    Ok(client)
}

type EventRow = (i64, &'static str, Option<String>, String, i64);

/// Bind parameters for `append`'s statement: the execution id, the projection
/// (when this statement writes it), then five per event.
fn bind<'a>(
    id: &'a String,
    projection: Option<&[&'a (dyn postgres::types::ToSql + Sync); 7]>,
    chunk: &'a [EventRow],
) -> Vec<&'a (dyn postgres::types::ToSql + Sync)> {
    let mut v: Vec<&'a (dyn postgres::types::ToSql + Sync)> =
        Vec::with_capacity(8 + chunk.len() * 5);
    v.push(id);
    if let Some(p) = projection {
        v.extend(p.iter().copied());
    }
    for (seq, ty, step, payload, ts) in chunk {
        v.extend([
            seq as &(dyn postgres::types::ToSql + Sync),
            ty,
            step,
            payload,
            ts,
        ]);
    }
    v
}

/// Like [`connect_client`], with the schema created under an advisory lock:
/// shared-mode servers usually start together, and concurrent
/// `CREATE TABLE IF NOT EXISTS` statements can fail.
fn connect_client_serialized(url: &str) -> Result<Client> {
    let mut client = Client::connect(url, tls()?)?;
    let mut tx = client.transaction()?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&BASE_SCHEMA_LOCK_KEY])?;
    tx.batch_execute(SCHEMA)?;
    if tx
        .query_one("SELECT COUNT(*) FROM schema_version", &[])?
        .get::<_, i64>(0)
        == 0
    {
        tx.execute("INSERT INTO schema_version (version) VALUES (1)", &[])?;
    }
    tx.commit()?;
    Ok(client)
}

/// Advisory lock key serialising creation of the base schema (shared mode).
const BASE_SCHEMA_LOCK_KEY: i64 = 0x6f72_6269_7473_6362;

fn bad_payload(e: serde_json::Error) -> StoreError {
    StoreError(format!("postgres: unreadable event payload: {e}"))
}

impl PgStore {
    pub fn connect(url: &str) -> Result<PgStore> {
        PgStore::connect_with(url, PgMode::ReadWrite)
    }

    /// Connect with a write mode (see [`PgMode`]).
    pub fn connect_with(url: &str, mode: PgMode) -> Result<PgStore> {
        let url = url.to_string();
        let (jobs, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        std::thread::Builder::new()
            .name("orbit-postgres".into())
            .spawn(move || {
                let open = |url: &str| match mode {
                    PgMode::Shared => connect_client_serialized(url),
                    _ => connect_client(url),
                };
                let mut client = match open(&url) {
                    Ok(c) => {
                        let _ = ready_tx.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                for job in rx {
                    if client.is_closed() {
                        match open(&url) {
                            Ok(c) => {
                                tracing::warn!("reconnected to PostgreSQL");
                                client = c;
                            }
                            Err(e) => tracing::error!(error = %e, "PostgreSQL reconnect failed"),
                        }
                    }
                    job(&mut client);
                }
            })
            .map_err(|e| StoreError(format!("cannot start PostgreSQL thread: {e}")))?;
        ready_rx
            .recv()
            .map_err(|_| StoreError("PostgreSQL thread exited during startup".into()))??;
        Ok(PgStore { jobs, mode })
    }

    pub(crate) fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Client) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = mpsc::channel();
        self.jobs
            .send(Box::new(move |c| {
                let _ = tx.send(f(c));
            }))
            .map_err(|_| StoreError("PostgreSQL connection thread has stopped".into()))?;
        rx.recv()
            .map_err(|_| StoreError("PostgreSQL connection thread has stopped".into()))?
    }

    pub fn mode(&self) -> PgMode {
        self.mode
    }

    pub fn put_definition(&self, ir: &WorkflowIr, now: u64) -> Result<PutDefinition> {
        if self.mode == PgMode::ReadOnly {
            return Err(read_only());
        }
        let (name, version, hash) = (
            ir.name.clone(),
            ir.version.clone(),
            ir.definition_hash.clone(),
        );
        let text = serde_json::to_string(ir).expect("IR serialises");
        self.run(move |c| {
            let existing = c.query_opt(
                "SELECT definition_hash FROM workflow_definitions WHERE name = $1 AND version = $2",
                &[&name, &version],
            )?;
            match existing.map(|r| r.get::<_, String>(0)) {
                Some(h) if h == hash => Ok(PutDefinition::Unchanged),
                Some(h) => Ok(PutDefinition::Conflict { existing_hash: h }),
                None => {
                    // ON CONFLICT DO NOTHING closes the race with a concurrent registration.
                    let n = c.execute(
                        "INSERT INTO workflow_definitions (name, version, definition_hash, ir, created_at_ms)
                         VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
                        &[&name, &version, &hash, &text, &(now as i64)],
                    )?;
                    Ok(if n == 1 { PutDefinition::Created } else { PutDefinition::Unchanged })
                }
            }
        })
    }

    pub fn definitions(&self) -> Result<Vec<WorkflowIr>> {
        let rows = self.run(|c| {
            Ok(c.query(
                "SELECT ir FROM workflow_definitions ORDER BY name, created_at_ms",
                &[],
            )?
            .into_iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>())
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|t| match serde_json::from_str::<WorkflowIr>(&t) {
                Ok(ir) => Some(ir),
                Err(e) => {
                    tracing::error!(error = %e, "skipping unreadable workflow definition");
                    None
                }
            })
            .collect())
    }

    pub fn execution_exists(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.run(move |c| {
            Ok(c.query_opt(
                "SELECT 1 FROM workflow_executions WHERE execution_id = $1",
                &[&id],
            )?
            .is_some())
        })
    }

    pub fn append(&mut self, state: &ExecutionState, events: &[HistoryEvent]) -> Result<()> {
        let fence = match self.mode {
            PgMode::ReadWrite => None,
            PgMode::ReadOnly => return Err(read_only()),
            PgMode::Fenced(term) => Some(term),
            PgMode::Shared => {
                return Err(StoreError(
                    "shared mode: history appends must carry the execution's ownership epoch"
                        .into(),
                ))
            }
        };
        let rows: Vec<EventRow> = events
            .iter()
            .map(|h| {
                (
                    h.seq as i64,
                    h.event.name(),
                    h.event.step().map(str::to_string),
                    serde_json::to_string(&h.event).expect("event serialises"),
                    h.timestamp_ms as i64,
                )
            })
            .collect();
        let id = state.id.clone();
        let projection = (
            state.workflow.clone(),
            state.version.clone(),
            state.definition_hash.clone(),
            state.status.as_str(),
            state.created_at_ms as i64,
            state.updated_at_ms as i64,
            state.last_seq as i64,
        );
        self.run(move |c| {
            type P<'a> = &'a (dyn postgres::types::ToSql + Sync);
            let (workflow, version, hash, status, created, updated, last_seq) = &projection;
            // Events (in chunks that stay well under PostgreSQL's 65535 bind
            // parameters) and the execution projection, written by one
            // statement: a data-modifying CTE. Unfenced, that statement is the
            // whole transaction, so a typical commit is one round trip.
            let statement = |chunk: &[EventRow],
                             with_projection: bool|
             -> String {
                let base = if with_projection { 8 } else { 1 };
                let mut sql = String::from(
                    "INSERT INTO execution_events (execution_id, seq, event_type, step, payload, timestamp_ms) VALUES ",
                );
                for i in 0..chunk.len() {
                    let b = base + 1 + i * 5;
                    if i > 0 {
                        sql.push(',');
                    }
                    sql.push_str(&format!(
                        "($1, ${}, ${}, ${}, ${}, ${})",
                        b,
                        b + 1,
                        b + 2,
                        b + 3,
                        b + 4
                    ));
                }
                if !with_projection {
                    return sql;
                }
                format!(
                    "WITH ev AS ({sql} RETURNING 1) \
                     INSERT INTO workflow_executions (execution_id, workflow, version, definition_hash, status, created_at_ms, updated_at_ms, last_seq) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                     ON CONFLICT (execution_id) DO UPDATE SET status = EXCLUDED.status, updated_at_ms = EXCLUDED.updated_at_ms, last_seq = EXCLUDED.last_seq"
                )
            };
            let proj: [P<'_>; 7] = [workflow, version, hash, status, created, updated, last_seq];
            const CHUNK: usize = 1000;
            if fence.is_none() && rows.len() <= CHUNK {
                c.execute(&statement(&rows, true), &bind(&id, Some(&proj), &rows))?;
                return Ok(());
            }
            let mut tx = c.transaction()?;
            if let Some(term) = fence {
                // FOR SHARE: a new leader's term increment waits for this
                // transaction, and this read sees any increment committed first.
                let current: Option<i64> = tx
                    .query_opt("SELECT term FROM orbit_cluster WHERE id = 1 FOR SHARE", &[])?
                    .map(|r| r.get(0));
                if current != Some(term) {
                    return Err(StoreError(format!(
                        "fenced: this server led term {term}, but the cluster is at term {}; another server is the leader",
                        current.map_or("unknown".to_string(), |t| t.to_string())
                    )));
                }
            }
            let chunks: Vec<_> = rows.chunks(CHUNK).collect();
            for (i, chunk) in chunks.iter().enumerate() {
                let last = i + 1 == chunks.len();
                tx.execute(
                    &statement(chunk, last),
                    &bind(&id, last.then_some(&proj), chunk),
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn history(&self, id: &str) -> Result<Vec<HistoryEvent>> {
        let id = id.to_string();
        let rows = self.run(move |c| {
            Ok(c.query(
                "SELECT seq, timestamp_ms, payload FROM execution_events WHERE execution_id = $1 ORDER BY seq",
                &[&id],
            )?
            .into_iter()
            .map(|r| (r.get::<_, i64>(0), r.get::<_, i64>(1), r.get::<_, String>(2)))
            .collect::<Vec<_>>())
        })?;
        rows.into_iter()
            .map(|(seq, ts, payload)| {
                let event: Event = serde_json::from_str(&payload).map_err(bad_payload)?;
                Ok(HistoryEvent {
                    seq: seq as u64,
                    timestamp_ms: ts as u64,
                    event,
                })
            })
            .collect()
    }

    pub fn append_rollover(
        &mut self,
        state: &ExecutionState,
        events: &[HistoryEvent],
        rollover: Rollover<'_>,
    ) -> Result<()> {
        let fence = match self.mode {
            PgMode::ReadWrite => None,
            PgMode::ReadOnly => return Err(read_only()),
            PgMode::Fenced(term) => Some(term),
            PgMode::Shared => {
                return Err(StoreError(
                    "shared mode: history appends must carry the execution's ownership epoch"
                        .into(),
                ))
            }
        };
        let tail = event_rows(rollover.events);
        let rows = event_rows(events);
        let run = rollover.run as i64;
        let id = state.id.clone();
        let projection = projection_of(state);
        self.run(move |c| {
            let mut tx = c.transaction()?;
            if let Some(term) = fence {
                check_term(&mut tx, term)?;
            }
            archive_run(&mut tx, &id, run, &tail)?;
            insert_events(&mut tx, &id, &rows, &projection)?;
            tx.commit()?;
            Ok(())
        })
    }

    pub fn run_history(&self, id: &str, run: u32) -> Result<Vec<HistoryEvent>> {
        let id = id.to_string();
        let rows = self.run(move |c| {
            Ok(c.query(
                "SELECT seq, timestamp_ms, payload FROM execution_run_events WHERE execution_id = $1 AND run = $2 ORDER BY seq",
                &[&id, &(run as i64)],
            )?
            .into_iter()
            .map(|r| (r.get::<_, i64>(0), r.get::<_, i64>(1), r.get::<_, String>(2)))
            .collect::<Vec<_>>())
        })?;
        rows.into_iter()
            .map(|(seq, ts, payload)| {
                let event: Event = serde_json::from_str(&payload).map_err(bad_payload)?;
                Ok(HistoryEvent {
                    seq: seq as u64,
                    timestamp_ms: ts as u64,
                    event,
                })
            })
            .collect()
    }

    pub fn execution_status(&self, id: &str) -> Result<Option<String>> {
        let id = id.to_string();
        self.run(move |c| {
            Ok(c.query_opt(
                "SELECT status FROM workflow_executions WHERE execution_id = $1",
                &[&id],
            )?
            .map(|r| r.get(0)))
        })
    }

    pub fn active_execution_ids(&self) -> Result<Vec<String>> {
        if matches!(self.mode, PgMode::ReadOnly | PgMode::Shared) {
            return Ok(vec![]);
        }
        self.run(|c| {
            Ok(c.query(
                "SELECT execution_id FROM workflow_executions WHERE status IN ('RUNNING', 'COMPENSATING') ORDER BY created_at_ms",
                &[],
            )?
            .into_iter()
            .map(|r| r.get(0))
            .collect())
        })
    }

    pub fn list_executions(
        &self,
        status: Option<&str>,
        workflow: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ExecutionSummary>> {
        let (status, workflow) = (status.map(str::to_string), workflow.map(str::to_string));
        self.run(move |c| {
            Ok(c.query(
                "SELECT execution_id, workflow, version, status, created_at_ms, updated_at_ms FROM workflow_executions
                 WHERE ($1::TEXT IS NULL OR status = $1) AND ($2::TEXT IS NULL OR workflow = $2)
                 ORDER BY created_at_ms DESC LIMIT $3",
                &[&status, &workflow, &(limit as i64)],
            )?
            .into_iter()
            .map(|r| ExecutionSummary {
                execution_id: r.get(0),
                workflow: r.get(1),
                version: r.get(2),
                status: r.get(3),
                created_at_ms: r.get::<_, i64>(4) as u64,
                updated_at_ms: r.get::<_, i64>(5) as u64,
            })
            .collect())
        })
    }

    pub fn count_by_status(&self) -> Result<Vec<(String, u64)>> {
        self.run(|c| {
            Ok(c.query(
                "SELECT status, COUNT(*) FROM workflow_executions GROUP BY status",
                &[],
            )?
            .into_iter()
            .map(|r| (r.get(0), r.get::<_, i64>(1) as u64))
            .collect())
        })
    }

    pub fn upsert_worker(&self, w: &WorkerRecord) -> Result<()> {
        if self.mode == PgMode::ReadOnly {
            return Err(read_only());
        }
        let w = w.clone();
        self.run(move |c| {
            c.execute(
                "INSERT INTO worker_registry (worker_id, runtime, sdk, contracts, registered_at_ms, last_seen_ms) VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (worker_id) DO UPDATE SET runtime = EXCLUDED.runtime, sdk = EXCLUDED.sdk, contracts = EXCLUDED.contracts,
                 registered_at_ms = EXCLUDED.registered_at_ms, last_seen_ms = EXCLUDED.last_seen_ms",
                &[&w.worker_id, &w.runtime, &w.sdk, &w.contracts.to_string(), &(w.registered_at_ms as i64), &(w.last_seen_ms as i64)],
            )?;
            Ok(())
        })
    }

    pub fn touch_worker(&self, worker_id: &str, now: u64) -> Result<()> {
        if self.mode == PgMode::ReadOnly {
            return Err(read_only());
        }
        let id = worker_id.to_string();
        self.run(move |c| {
            c.execute(
                "UPDATE worker_registry SET last_seen_ms = $2 WHERE worker_id = $1",
                &[&id, &(now as i64)],
            )?;
            Ok(())
        })
    }

    pub fn workers(&self) -> Result<Vec<WorkerRecord>> {
        self.run(|c| {
            Ok(c.query(
                "SELECT worker_id, runtime, sdk, contracts, registered_at_ms, last_seen_ms FROM worker_registry ORDER BY worker_id",
                &[],
            )?
            .into_iter()
            .map(|r| WorkerRecord {
                worker_id: r.get(0),
                runtime: r.get(1),
                sdk: r.get(2),
                contracts: serde_json::from_str(&r.get::<_, String>(3)).unwrap_or_default(),
                registered_at_ms: r.get::<_, i64>(4) as u64,
                last_seen_ms: r.get::<_, i64>(5) as u64,
            })
            .collect())
        })
    }
}

type Projection = (String, String, String, &'static str, i64, i64, i64);

fn projection_of(state: &ExecutionState) -> Projection {
    (
        state.workflow.clone(),
        state.version.clone(),
        state.definition_hash.clone(),
        state.status.as_str(),
        state.created_at_ms as i64,
        state.updated_at_ms as i64,
        state.last_seq as i64,
    )
}

fn event_rows(events: &[HistoryEvent]) -> Vec<EventRow> {
    events
        .iter()
        .map(|h| {
            (
                h.seq as i64,
                h.event.name(),
                h.event.step().map(str::to_string),
                serde_json::to_string(&h.event).expect("event serialises"),
                h.timestamp_ms as i64,
            )
        })
        .collect()
}

/// The cluster term check of a fenced append (see [`PgStore::append`]).
fn check_term(tx: &mut postgres::Transaction<'_>, term: i64) -> Result<()> {
    let current: Option<i64> = tx
        .query_opt("SELECT term FROM orbit_cluster WHERE id = 1 FOR SHARE", &[])?
        .map(|r| r.get(0));
    if current != Some(term) {
        return Err(StoreError(format!(
            "fenced: this server led term {term}, but the cluster is at term {}; another server is the leader",
            current.map_or("unknown".to_string(), |t| t.to_string())
        )));
    }
    Ok(())
}

/// Continue-as-new: move the stored history of the finished run, plus its
/// last events `tail`, to `execution_run_events` under `run`.
fn archive_run(
    tx: &mut postgres::Transaction<'_>,
    id: &String,
    run: i64,
    tail: &[EventRow],
) -> Result<()> {
    tx.execute(
        "INSERT INTO execution_run_events (execution_id, run, seq, event_type, step, payload, timestamp_ms)
         SELECT execution_id, $2, seq, event_type, step, payload, timestamp_ms FROM execution_events WHERE execution_id = $1",
        &[id, &run],
    )?;
    tx.execute(
        "DELETE FROM execution_events WHERE execution_id = $1",
        &[id],
    )?;
    let ins = tx.prepare(
        "INSERT INTO execution_run_events (execution_id, run, seq, event_type, step, payload, timestamp_ms)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )?;
    for (seq, ty, step, payload, ts) in tail {
        tx.execute(&ins, &[id, &run, seq, ty, step, payload, ts])?;
    }
    Ok(())
}

/// Insert events one by one and upsert the projection (used by the
/// transactional paths that are not the hot single-statement append).
fn insert_events(
    tx: &mut postgres::Transaction<'_>,
    id: &String,
    rows: &[EventRow],
    projection: &Projection,
) -> Result<()> {
    let ins = tx.prepare(
        "INSERT INTO execution_events (execution_id, seq, event_type, step, payload, timestamp_ms)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )?;
    for (seq, ty, step, payload, ts) in rows {
        tx.execute(&ins, &[id, seq, ty, step, payload, ts])?;
    }
    let (workflow, version, hash, status, created, updated, last_seq) = projection;
    tx.execute(
        "INSERT INTO workflow_executions (execution_id, workflow, version, definition_hash, status, created_at_ms, updated_at_ms, last_seq)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (execution_id) DO UPDATE SET status = EXCLUDED.status, updated_at_ms = EXCLUDED.updated_at_ms, last_seq = EXCLUDED.last_seq",
        &[id, workflow, version, hash, status, created, updated, last_seq],
    )?;
    Ok(())
}

// -------------------------------------------------------------- schedules

const SCHEDULE_COLUMNS: &str = "schedule_id, spec, paused, next_fire_ms, due_ms, last_fire_ms, last_execution_id, fired_count, skipped_count, last_error, created_at_ms, updated_at_ms";

fn schedule_row(r: &postgres::Row) -> ScheduleRow {
    let opt = |v: Option<i64>| v.map(|x| x as u64);
    ScheduleRow {
        schedule_id: r.get(0),
        spec: r.get(1),
        paused: r.get::<_, i64>(2) != 0,
        next_fire_ms: opt(r.get(3)),
        due_ms: opt(r.get(4)),
        last_fire_ms: opt(r.get(5)),
        last_execution_id: r.get(6),
        fired: r.get::<_, i64>(7) as u64,
        skipped: r.get::<_, i64>(8) as u64,
        last_error: r.get(9),
        created_at_ms: r.get::<_, i64>(10) as u64,
        updated_at_ms: r.get::<_, i64>(11) as u64,
    }
}

fn opt_i64(v: Option<u64>) -> Option<i64> {
    v.map(|x| x as i64)
}

impl PgStore {
    fn writable(&self) -> Result<Option<i64>> {
        match self.mode {
            PgMode::ReadOnly => Err(read_only()),
            PgMode::Fenced(term) => Ok(Some(term)),
            _ => Ok(None),
        }
    }

    pub fn insert_schedule(&self, row: &ScheduleRow) -> Result<bool> {
        self.writable()?;
        let r = row.clone();
        self.run(move |c| {
            let n = c.execute(
                &format!(
                    "INSERT INTO orbit_schedules ({SCHEDULE_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
                     ON CONFLICT (schedule_id) DO NOTHING"
                ),
                &[
                    &r.schedule_id,
                    &r.spec,
                    &(r.paused as i64),
                    &opt_i64(r.next_fire_ms),
                    &opt_i64(r.due_ms),
                    &opt_i64(r.last_fire_ms),
                    &r.last_execution_id,
                    &(r.fired as i64),
                    &(r.skipped as i64),
                    &r.last_error,
                    &(r.created_at_ms as i64),
                    &(r.updated_at_ms as i64),
                ],
            )?;
            Ok(n == 1)
        })
    }

    pub fn update_schedule(&self, row: &ScheduleRow) -> Result<bool> {
        self.writable()?;
        let r = row.clone();
        self.run(move |c| {
            let n = c.execute(
                "UPDATE orbit_schedules SET spec = $2, paused = $3, next_fire_ms = $4, due_ms = $5, last_error = $6,
                 claimed_by = NULL, claimed_until_ms = NULL, updated_at_ms = $7 WHERE schedule_id = $1",
                &[
                    &r.schedule_id,
                    &r.spec,
                    &(r.paused as i64),
                    &opt_i64(r.next_fire_ms),
                    &opt_i64(r.due_ms),
                    &r.last_error,
                    &(r.updated_at_ms as i64),
                ],
            )?;
            Ok(n == 1)
        })
    }

    pub fn schedule(&self, id: &str) -> Result<Option<ScheduleRow>> {
        let id = id.to_string();
        self.run(move |c| {
            Ok(c.query_opt(
                &format!("SELECT {SCHEDULE_COLUMNS} FROM orbit_schedules WHERE schedule_id = $1"),
                &[&id],
            )?
            .map(|r| schedule_row(&r)))
        })
    }

    pub fn schedules(&self) -> Result<Vec<ScheduleRow>> {
        self.run(|c| {
            Ok(c.query(
                &format!("SELECT {SCHEDULE_COLUMNS} FROM orbit_schedules ORDER BY schedule_id"),
                &[],
            )?
            .iter()
            .map(schedule_row)
            .collect())
        })
    }

    pub fn delete_schedule(&self, id: &str) -> Result<bool> {
        self.writable()?;
        let id = id.to_string();
        self.run(move |c| {
            Ok(c.execute("DELETE FROM orbit_schedules WHERE schedule_id = $1", &[&id])? == 1)
        })
    }

    pub fn due_schedules(&self, now: u64) -> Result<Vec<ScheduleRow>> {
        let now = now as i64;
        self.run(move |c| {
            Ok(c.query(
                &format!(
                    "SELECT {SCHEDULE_COLUMNS} FROM orbit_schedules
                     WHERE paused = 0 AND due_ms IS NOT NULL AND due_ms <= $1
                     AND (claimed_until_ms IS NULL OR claimed_until_ms <= $1)
                     ORDER BY due_ms LIMIT 100"
                ),
                &[&now],
            )?
            .iter()
            .map(schedule_row)
            .collect())
        })
    }

    pub fn claim_schedule(
        &self,
        id: &str,
        next_fire_ms: u64,
        node: &str,
        now: u64,
        until: u64,
    ) -> Result<bool> {
        let fence = self.writable()?;
        let (id, node) = (id.to_string(), node.to_string());
        self.run(move |c| {
            let mut tx = c.transaction()?;
            if let Some(term) = fence {
                // A deposed leader must not take (and then skip) occurrences.
                check_term(&mut tx, term)?;
            }
            let n = tx.execute(
                "UPDATE orbit_schedules SET claimed_by = $3, claimed_until_ms = $5
                 WHERE schedule_id = $1 AND next_fire_ms = $2 AND paused = 0
                 AND (claimed_until_ms IS NULL OR claimed_until_ms <= $4)",
                &[
                    &id,
                    &(next_fire_ms as i64),
                    &node,
                    &(now as i64),
                    &(until as i64),
                ],
            )?;
            tx.commit()?;
            Ok(n == 1)
        })
    }

    pub fn finish_schedule_fire(
        &self,
        id: &str,
        next_fire_ms: u64,
        node: &str,
        now: u64,
        rec: &FireRecord,
    ) -> Result<bool> {
        let fence = self.writable()?;
        let (id, node, rec) = (id.to_string(), node.to_string(), rec.clone());
        self.run(move |c| {
            let mut tx = c.transaction()?;
            if let Some(term) = fence {
                check_term(&mut tx, term)?;
            }
            let n = tx.execute(
                "UPDATE orbit_schedules SET next_fire_ms = $4, due_ms = $5,
                 last_fire_ms = COALESCE($6, last_fire_ms), last_execution_id = COALESCE($7, last_execution_id),
                 fired_count = fired_count + $8, skipped_count = skipped_count + $9, last_error = $10,
                 claimed_by = NULL, claimed_until_ms = NULL, updated_at_ms = $11
                 WHERE schedule_id = $1 AND next_fire_ms = $2 AND claimed_by = $3",
                &[
                    &id,
                    &(next_fire_ms as i64),
                    &node,
                    &opt_i64(rec.next_fire_ms),
                    &opt_i64(rec.due_ms),
                    &opt_i64(rec.last_fire_ms),
                    &rec.last_execution_id,
                    &(rec.fired as i64),
                    &(rec.skipped as i64),
                    &rec.last_error,
                    &(now as i64),
                ],
            )?;
            tx.commit()?;
            Ok(n == 1)
        })
    }

    pub fn release_schedule_claims(&self) -> Result<u64> {
        self.writable()?;
        self.run(|c| {
            Ok(c.execute(
                "UPDATE orbit_schedules SET claimed_by = NULL, claimed_until_ms = NULL WHERE claimed_by IS NOT NULL",
                &[],
            )?)
        })
    }

    pub fn release_schedule_claim(
        &self,
        id: &str,
        node: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<()> {
        self.writable()?;
        let (id, node, error) = (id.to_string(), node.to_string(), error.to_string());
        self.run(move |c| {
            c.execute(
                "UPDATE orbit_schedules SET claimed_by = NULL, claimed_until_ms = $3, last_error = $4, updated_at_ms = $5
                 WHERE schedule_id = $1 AND claimed_by = $2",
                &[&id, &node, &(retry_at as i64), &error, &(now as i64)],
            )?;
            Ok(())
        })
    }

    pub fn record_schedule_trigger(&self, id: &str, execution_id: &str, now: u64) -> Result<()> {
        self.writable()?;
        let (id, exec) = (id.to_string(), execution_id.to_string());
        self.run(move |c| {
            c.execute(
                "UPDATE orbit_schedules SET last_execution_id = $2, fired_count = fired_count + 1, updated_at_ms = $3
                 WHERE schedule_id = $1",
                &[&id, &exec, &(now as i64)],
            )?;
            Ok(())
        })
    }
}

// ------------------------------------------------------------- shared mode

/// Database time in milliseconds. Shared mode measures node heartbeats and
/// worker session liveness with it, so clock differences between servers do
/// not decide who is alive.
pub(crate) const NOW_MS: &str = "(extract(epoch from clock_timestamp()) * 1000)::bigint";

/// Tables of shared mode (created by `crate::shared`). See
/// `docs/src/runtime/storage.md` for what each column means.
pub(crate) const SHARED_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orbit_nodes (
    node_id TEXT PRIMARY KEY,
    incarnation TEXT NOT NULL,
    addr TEXT,
    heartbeat_ms BIGINT NOT NULL,
    started_ms BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS orbit_owners (
    execution_id TEXT PRIMARY KEY,
    root_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    epoch BIGINT NOT NULL,
    since_ms BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_orbit_owners_node ON orbit_owners(node_id);
CREATE INDEX IF NOT EXISTS idx_orbit_owners_root ON orbit_owners(root_id);
CREATE TABLE IF NOT EXISTS orbit_tasks (
    id BIGSERIAL PRIMARY KEY,
    execution_id TEXT NOT NULL,
    unit_key TEXT NOT NULL,
    attempt BIGINT NOT NULL,
    offer BIGINT NOT NULL,
    task_name TEXT NOT NULL,
    contract_version TEXT NOT NULL,
    language TEXT,
    payload TEXT NOT NULL,
    enqueued_ms BIGINT NOT NULL,
    state TEXT NOT NULL,
    token TEXT,
    claimed_by TEXT,
    claimed_inc TEXT,
    claimed_session TEXT,
    claimed_worker TEXT,
    claimed_ms BIGINT,
    updated_ms BIGINT NOT NULL,
    UNIQUE (execution_id, unit_key, attempt, offer)
);
CREATE INDEX IF NOT EXISTS idx_orbit_tasks_queued ON orbit_tasks(task_name, enqueued_ms) WHERE state = 'queued';
CREATE INDEX IF NOT EXISTS idx_orbit_tasks_token ON orbit_tasks(token);
CREATE INDEX IF NOT EXISTS idx_orbit_tasks_session ON orbit_tasks(claimed_session);
CREATE TABLE IF NOT EXISTS orbit_inbox (
    seq BIGSERIAL PRIMARY KEY,
    execution_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    token TEXT,
    payload TEXT NOT NULL,
    created_ms BIGINT NOT NULL,
    done_ms BIGINT,
    result TEXT
);
CREATE INDEX IF NOT EXISTS idx_orbit_inbox_execution ON orbit_inbox(execution_id, seq);
CREATE TABLE IF NOT EXISTS orbit_sessions (
    session_id TEXT PRIMARY KEY,
    worker_id TEXT NOT NULL,
    runtime TEXT NOT NULL,
    contracts TEXT NOT NULL,
    node_id TEXT NOT NULL,
    registered_ms BIGINT NOT NULL,
    last_seen_ms BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_orbit_sessions_worker ON orbit_sessions(worker_id);
"#;

/// A worker task to put on the shared queue (one row per offer of a unit).
#[derive(Debug, Clone)]
pub(crate) struct TaskOffer {
    pub unit_key: String,
    pub attempt: i64,
    /// The unit's dispatch count when it was scheduled: together with the
    /// attempt it identifies this offer, so a claim of an older offer of the
    /// same unit is recognised as stale.
    pub offer: i64,
    pub task_name: String,
    pub contract_version: String,
    pub language: Option<String>,
    /// The task assignment without token and deadline (JSON).
    pub payload: String,
    pub enqueued_ms: i64,
}

/// Queue rows of a unit to withdraw: every queued or claimed row of the unit
/// except the offer in `keep` (attempt, offer) becomes `cancelled`, which
/// tells the worker holding it to stop at its next heartbeat.
#[derive(Debug, Clone)]
pub(crate) struct UnitSync {
    pub unit_key: String,
    pub keep: Option<(i64, i64)>,
}

/// Everything shared mode writes in the same transaction as a history append.
#[derive(Debug, Clone, Default)]
pub(crate) struct SharedAppend {
    pub node_id: String,
    /// The ownership epoch this node holds for the execution; `None` for a
    /// new execution, whose ownership row is created at epoch 1.
    pub epoch: Option<i64>,
    pub parent: Option<String>,
    pub offers: Vec<TaskOffer>,
    pub syncs: Vec<UnitSync>,
    /// Tokens whose result arrived: their queue rows become `done`.
    pub done_tokens: Vec<String>,
    /// Inbox entries applied by this append: (seq, reply). Entries without a
    /// reply are deleted; the others are marked done with the reply.
    pub consumed: Vec<(i64, Option<String>)>,
    /// Set when the execution reaches a terminal status: the reply recorded
    /// for commands still waiting in its inbox.
    pub finished_reply: Option<String>,
    /// Continue-as-new: the finished run and its last events, archived
    /// before the new run's events are written. The execution keeps its
    /// owner and epoch; the finished run's queue rows are deleted.
    pub rollover: Option<(u32, Vec<HistoryEvent>)>,
}

impl PgStore {
    /// Shared-mode append: checks that this node still owns the execution at
    /// its epoch (`SELECT ... FOR SHARE`, the same pattern as the cluster
    /// term), then writes the events, the projection and the queue and inbox
    /// changes in one transaction.
    pub(crate) fn append_shared(
        &mut self,
        state: &ExecutionState,
        events: &[HistoryEvent],
        ext: SharedAppend,
    ) -> Result<()> {
        if self.mode != PgMode::Shared {
            return Err(StoreError("append_shared needs a shared-mode store".into()));
        }
        let rows: Vec<(i64, &'static str, Option<String>, String, i64)> = events
            .iter()
            .map(|h| {
                (
                    h.seq as i64,
                    h.event.name(),
                    h.event.step().map(str::to_string),
                    serde_json::to_string(&h.event).expect("event serialises"),
                    h.timestamp_ms as i64,
                )
            })
            .collect();
        let id = state.id.clone();
        let projection = (
            state.workflow.clone(),
            state.version.clone(),
            state.definition_hash.clone(),
            state.status.as_str(),
            state.created_at_ms as i64,
            state.updated_at_ms as i64,
            state.last_seq as i64,
        );
        let now = state.updated_at_ms as i64;
        self.run(move |c| {
            let mut tx = c.transaction()?;
            let node = &ext.node_id;
            match ext.epoch {
                Some(epoch) => {
                    // FOR SHARE: a takeover's epoch increment waits for this
                    // transaction, and this read sees any increment committed first.
                    let owner: Option<(String, i64)> = tx
                        .query_opt(
                            "SELECT node_id, epoch FROM orbit_owners WHERE execution_id = $1 FOR SHARE",
                            &[&id],
                        )?
                        .map(|r| (r.get(0), r.get(1)));
                    if owner.as_ref() != Some(&(node.clone(), epoch)) {
                        return Err(StoreError(match owner {
                            Some((n, e)) => format!(
                                "fenced: execution `{id}` is owned by node `{n}` at epoch {e}; this node held epoch {epoch}"
                            ),
                            None => format!(
                                "fenced: execution `{id}` has no owner any more; this node held epoch {epoch}"
                            ),
                        }));
                    }
                }
                None => {
                    let created = tx.execute(
                        &format!(
                            "INSERT INTO orbit_owners (execution_id, root_id, node_id, epoch, since_ms)
                             VALUES ($1, COALESCE((SELECT root_id FROM orbit_owners WHERE execution_id = $2), $1), $3, 1, {NOW_MS})
                             ON CONFLICT DO NOTHING"
                        ),
                        &[&id, &ext.parent, node],
                    )?;
                    if created == 0 {
                        return Err(StoreError(format!(
                            "fenced: execution `{id}` was created by another server at the same time"
                        )));
                    }
                }
            }
            if let Some((run, tail)) = &ext.rollover {
                archive_run(&mut tx, &id, *run as i64, &event_rows(tail))?;
                tx.execute("DELETE FROM orbit_tasks WHERE execution_id = $1", &[&id])?;
            }
            let ins = tx.prepare(
                "INSERT INTO execution_events (execution_id, seq, event_type, step, payload, timestamp_ms)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )?;
            for (seq, ty, step, payload, ts) in &rows {
                tx.execute(&ins, &[&id, seq, ty, step, payload, ts])?;
            }
            let (workflow, version, hash, status, created, updated, last_seq) = projection;
            tx.execute(
                "INSERT INTO workflow_executions (execution_id, workflow, version, definition_hash, status, created_at_ms, updated_at_ms, last_seq)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (execution_id) DO UPDATE SET status = EXCLUDED.status, updated_at_ms = EXCLUDED.updated_at_ms, last_seq = EXCLUDED.last_seq",
                &[&id, &workflow, &version, &hash, &status, &created, &updated, &last_seq],
            )?;
            if !ext.done_tokens.is_empty() {
                tx.execute(
                    "UPDATE orbit_tasks SET state = 'done', updated_ms = $2 WHERE token = ANY($1) AND state IN ('claimed', 'cancelled')",
                    &[&ext.done_tokens, &now],
                )?;
            }
            if !ext.offers.is_empty() {
                let ins = tx.prepare(
                    "INSERT INTO orbit_tasks (execution_id, unit_key, attempt, offer, task_name, contract_version, language, payload, enqueued_ms, state, updated_ms)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'queued', $10)
                     ON CONFLICT (execution_id, unit_key, attempt, offer) DO NOTHING",
                )?;
                for o in &ext.offers {
                    tx.execute(
                        &ins,
                        &[&id, &o.unit_key, &o.attempt, &o.offer, &o.task_name, &o.contract_version, &o.language, &o.payload, &o.enqueued_ms, &now],
                    )?;
                }
            }
            if !ext.syncs.is_empty() {
                let upd = tx.prepare(
                    "UPDATE orbit_tasks SET state = 'cancelled', updated_ms = $5
                     WHERE execution_id = $1 AND unit_key = $2 AND state IN ('queued', 'claimed') AND NOT (attempt = $3 AND offer = $4)",
                )?;
                for s in &ext.syncs {
                    let (a, o) = s.keep.unwrap_or((-1, -1));
                    tx.execute(&upd, &[&id, &s.unit_key, &a, &o, &now])?;
                }
            }
            for (seq, reply) in &ext.consumed {
                match reply {
                    None => tx.execute("DELETE FROM orbit_inbox WHERE seq = $1", &[seq])?,
                    Some(r) => tx.execute(
                        "UPDATE orbit_inbox SET done_ms = $2, result = $3 WHERE seq = $1",
                        &[seq, &now, r],
                    )?,
                };
            }
            if let Some(reply) = &ext.finished_reply {
                tx.execute(
                    "UPDATE orbit_tasks SET state = 'cancelled', updated_ms = $2 WHERE execution_id = $1 AND state IN ('queued', 'claimed')",
                    &[&id, &now],
                )?;
                tx.execute("DELETE FROM orbit_owners WHERE execution_id = $1", &[&id])?;
                tx.execute(
                    "UPDATE orbit_inbox SET done_ms = $2, result = $3 WHERE execution_id = $1 AND done_ms IS NULL AND kind IN ('signal', 'cancel')",
                    &[&id, &now, reply],
                )?;
                tx.execute(
                    "DELETE FROM orbit_inbox WHERE execution_id = $1 AND done_ms IS NULL",
                    &[&id],
                )?;
                tx.execute("SELECT pg_notify('orbit_done', $1)", &[&id])?;
            }
            if !ext.offers.is_empty() {
                tx.execute("SELECT pg_notify('orbit_tasks', '')", &[])?;
            }
            tx.commit()?;
            Ok(())
        })
    }
}
