//! PostgreSQL backend.
//!
//! The `postgres` crate drives its own async runtime internally and must not be
//! called from inside another one, while the engine runs synchronously inside
//! the HTTP server's tokio runtime. So every statement runs on one dedicated
//! connection thread, fed through a channel. Calls stay synchronous for the
//! engine, the connection is reconnected automatically if it drops, and each
//! `append` is a single transaction, exactly like the SQLite backend.

use super::{ExecutionSummary, PutDefinition, Result, StoreError, WorkerRecord, SCHEMA};
use crate::model::{Event, ExecutionState, HistoryEvent};
use orbit_ir::WorkflowIr;
use postgres::{Client, NoTls};
use std::sync::mpsc;

type Job = Box<dyn FnOnce(&mut Client) + Send>;

pub struct PgStore {
    jobs: mpsc::Sender<Job>,
}

fn connect_client(url: &str) -> Result<Client> {
    let mut client = Client::connect(url, NoTls)?;
    client.batch_execute(SCHEMA)?;
    let rows = client.query_one("SELECT COUNT(*) FROM schema_version", &[])?;
    if rows.get::<_, i64>(0) == 0 {
        client.execute("INSERT INTO schema_version (version) VALUES (1)", &[])?;
    }
    Ok(client)
}

fn bad_payload(e: serde_json::Error) -> StoreError {
    StoreError(format!("postgres: unreadable event payload: {e}"))
}

impl PgStore {
    pub fn connect(url: &str) -> Result<PgStore> {
        let url = url.to_string();
        let (jobs, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        std::thread::Builder::new()
            .name("orbit-postgres".into())
            .spawn(move || {
                let mut client = match connect_client(&url) {
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
                        match connect_client(&url) {
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
        Ok(PgStore { jobs })
    }

    fn run<T: Send + 'static>(
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

    pub fn put_definition(&self, ir: &WorkflowIr, now: u64) -> Result<PutDefinition> {
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
        self.run(move |c| {
            let mut tx = c.transaction()?;
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

    pub fn active_execution_ids(&self) -> Result<Vec<String>> {
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
