//! SQLite backend (default; a single file, WAL mode with `synchronous=FULL`).

use super::{ExecutionSummary, PutDefinition, Result, WorkerRecord, SCHEMA};
use crate::model::{Event, ExecutionState, HistoryEvent};
use orbit_ir::WorkflowIr;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<SqliteStore> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<SqliteStore> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<SqliteStore> {
        // WAL + FULL sync: a committed event survives power loss.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(&SCHEMA.replace("BIGINT", "INTEGER"))?;
        if conn.query_row("SELECT COUNT(*) FROM schema_version", [], |r| {
            r.get::<_, i64>(0)
        })? == 0
        {
            conn.execute("INSERT INTO schema_version (version) VALUES (1)", [])?;
        }
        Ok(SqliteStore { conn })
    }

    // ------------------------------------------------------------ definitions

    pub fn put_definition(&self, ir: &WorkflowIr, now: u64) -> Result<PutDefinition> {
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT definition_hash FROM workflow_definitions WHERE name = ?1 AND version = ?2",
                params![ir.name, ir.version],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(h) if h == ir.definition_hash => Ok(PutDefinition::Unchanged),
            Some(h) => Ok(PutDefinition::Conflict { existing_hash: h }),
            None => {
                let text = serde_json::to_string(ir).expect("IR serialises");
                self.conn.execute(
                    "INSERT INTO workflow_definitions (name, version, definition_hash, ir, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![ir.name, ir.version, ir.definition_hash, text, now as i64],
                )?;
                Ok(PutDefinition::Created)
            }
        }
    }

    pub fn definitions(&self) -> Result<Vec<WorkflowIr>> {
        let mut stmt = self
            .conn
            .prepare("SELECT ir FROM workflow_definitions ORDER BY name, created_at_ms")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for text in rows {
            match serde_json::from_str::<WorkflowIr>(&text?) {
                Ok(ir) => out.push(ir),
                Err(e) => tracing::error!(error = %e, "skipping unreadable workflow definition"),
            }
        }
        Ok(out)
    }

    // ------------------------------------------------------------- executions

    pub fn execution_exists(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM workflow_executions WHERE execution_id = ?1",
                params![id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Atomically append events and update the execution projection.
    pub fn append(&mut self, state: &ExecutionState, events: &[HistoryEvent]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut ins = tx.prepare_cached(
                "INSERT INTO execution_events (execution_id, seq, event_type, step, payload, timestamp_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for h in events {
                let payload = serde_json::to_string(&h.event).expect("event serialises");
                ins.execute(params![
                    state.id,
                    h.seq as i64,
                    h.event.name(),
                    h.event.step(),
                    payload,
                    h.timestamp_ms as i64
                ])?;
            }
        }
        tx.execute(
            "INSERT INTO workflow_executions (execution_id, workflow, version, definition_hash, status, created_at_ms, updated_at_ms, last_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(execution_id) DO UPDATE SET status = excluded.status, updated_at_ms = excluded.updated_at_ms, last_seq = excluded.last_seq",
            params![
                state.id,
                state.workflow,
                state.version,
                state.definition_hash,
                state.status.as_str(),
                state.created_at_ms as i64,
                state.updated_at_ms as i64,
                state.last_seq as i64
            ],
        )?;
        Ok(tx.commit()?)
    }

    pub fn history(&self, id: &str) -> Result<Vec<HistoryEvent>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT seq, timestamp_ms, payload FROM execution_events WHERE execution_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = vec![];
        for row in rows {
            let (seq, ts, payload) = row?;
            let event: Event = serde_json::from_str(&payload).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            out.push(HistoryEvent {
                seq: seq as u64,
                timestamp_ms: ts as u64,
                event,
            });
        }
        Ok(out)
    }

    /// Ids of executions that have not reached a terminal status.
    pub fn active_execution_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT execution_id FROM workflow_executions WHERE status IN ('RUNNING', 'COMPENSATING') ORDER BY created_at_ms",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn list_executions(
        &self,
        status: Option<&str>,
        workflow: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ExecutionSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT execution_id, workflow, version, status, created_at_ms, updated_at_ms FROM workflow_executions
             WHERE (?1 IS NULL OR status = ?1) AND (?2 IS NULL OR workflow = ?2)
             ORDER BY created_at_ms DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![status, workflow, limit], |r| {
            Ok(ExecutionSummary {
                execution_id: r.get(0)?,
                workflow: r.get(1)?,
                version: r.get(2)?,
                status: r.get(3)?,
                created_at_ms: r.get::<_, i64>(4)? as u64,
                updated_at_ms: r.get::<_, i64>(5)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn count_by_status(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT status, COUNT(*) FROM workflow_executions GROUP BY status")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // ---------------------------------------------------------------- workers

    pub fn upsert_worker(&self, w: &WorkerRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO worker_registry (worker_id, runtime, sdk, contracts, registered_at_ms, last_seen_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(worker_id) DO UPDATE SET runtime = excluded.runtime, sdk = excluded.sdk, contracts = excluded.contracts,
             registered_at_ms = excluded.registered_at_ms, last_seen_ms = excluded.last_seen_ms",
            params![w.worker_id, w.runtime, w.sdk, w.contracts.to_string(), w.registered_at_ms as i64, w.last_seen_ms as i64],
        )?;
        Ok(())
    }

    pub fn touch_worker(&self, worker_id: &str, now: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE worker_registry SET last_seen_ms = ?2 WHERE worker_id = ?1",
            params![worker_id, now as i64],
        )?;
        Ok(())
    }

    pub fn workers(&self) -> Result<Vec<WorkerRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT worker_id, runtime, sdk, contracts, registered_at_ms, last_seen_ms FROM worker_registry ORDER BY worker_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(WorkerRecord {
                worker_id: r.get(0)?,
                runtime: r.get(1)?,
                sdk: r.get(2)?,
                contracts: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                registered_at_ms: r.get::<_, i64>(4)? as u64,
                last_seen_ms: r.get::<_, i64>(5)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}
