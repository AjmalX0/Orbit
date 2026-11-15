//! Persistence.
//!
//! The `execution_events` table is the source of truth. `workflow_executions`
//! is a projection kept in the same transaction for fast listing. Every batch
//! of events is appended atomically together with the projection update.
//!
//! Two backends share the schema and the guarantees:
//!
//! * **SQLite** (default): a single file, WAL mode, `synchronous=FULL`.
//! * **PostgreSQL**: pass a `postgres://` URL. All statements run on one
//!   dedicated connection thread, so the synchronous engine never blocks the
//!   async server runtime from inside it, and the connection is re-established
//!   automatically if it drops.

mod postgres;
mod sqlite;

use crate::model::{ExecutionState, HistoryEvent};
use orbit_ir::WorkflowIr;
use serde::Serialize;
use std::path::Path;

pub use self::postgres::PgStore;
pub use self::sqlite::SqliteStore;

#[derive(Debug)]
pub enum PutDefinition {
    Created,
    Unchanged,
    Conflict { existing_hash: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecutionSummary {
    pub execution_id: String,
    pub workflow: String,
    pub version: String,
    pub status: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerRecord {
    pub worker_id: String,
    pub runtime: String,
    pub sdk: Option<String>,
    pub contracts: serde_json::Value,
    pub last_seen_ms: u64,
    pub registered_at_ms: u64,
}

/// A storage error from either backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError(format!("sqlite: {e}"))
    }
}

impl From<::postgres::Error> for StoreError {
    fn from(e: ::postgres::Error) -> Self {
        StoreError(format!("postgres: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Shared schema. `BIGINT` is rewritten to `INTEGER` for SQLite.
pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL);
CREATE TABLE IF NOT EXISTS workflow_definitions (
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    definition_hash TEXT NOT NULL,
    ir TEXT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    PRIMARY KEY (name, version)
);
CREATE TABLE IF NOT EXISTS workflow_executions (
    execution_id TEXT PRIMARY KEY,
    workflow TEXT NOT NULL,
    version TEXT NOT NULL,
    definition_hash TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL,
    last_seq BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_executions_status ON workflow_executions(status);
CREATE TABLE IF NOT EXISTS execution_events (
    execution_id TEXT NOT NULL,
    seq BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    step TEXT,
    payload TEXT NOT NULL,
    timestamp_ms BIGINT NOT NULL,
    PRIMARY KEY (execution_id, seq)
);
CREATE TABLE IF NOT EXISTS worker_registry (
    worker_id TEXT PRIMARY KEY,
    runtime TEXT NOT NULL,
    sdk TEXT,
    contracts TEXT NOT NULL,
    registered_at_ms BIGINT NOT NULL,
    last_seen_ms BIGINT NOT NULL
);
"#;

/// The storage backend used by the engine.
pub enum Store {
    Sqlite(SqliteStore),
    Postgres(PgStore),
}

macro_rules! dispatch {
    ($self:ident, $s:ident => $call:expr) => {
        match $self {
            Store::Sqlite($s) => $call,
            Store::Postgres($s) => $call,
        }
    };
}

impl Store {
    /// Open a backend from a location: a `postgres://` / `postgresql://` URL,
    /// or a SQLite file path.
    pub fn connect(location: &str) -> Result<Store> {
        if location.starts_with("postgres://") || location.starts_with("postgresql://") {
            Ok(Store::Postgres(PgStore::connect(location)?))
        } else {
            Store::open(Path::new(location))
        }
    }

    /// Open (or create) a SQLite database file.
    pub fn open(path: &Path) -> Result<Store> {
        Ok(Store::Sqlite(SqliteStore::open(path)?))
    }

    pub fn in_memory() -> Result<Store> {
        Ok(Store::Sqlite(SqliteStore::in_memory()?))
    }

    pub fn backend(&self) -> &'static str {
        match self {
            Store::Sqlite(_) => "sqlite",
            Store::Postgres(_) => "postgres",
        }
    }

    pub fn put_definition(&self, ir: &WorkflowIr, now: u64) -> Result<PutDefinition> {
        dispatch!(self, s => s.put_definition(ir, now))
    }

    pub fn definitions(&self) -> Result<Vec<WorkflowIr>> {
        dispatch!(self, s => s.definitions())
    }

    pub fn execution_exists(&self, id: &str) -> Result<bool> {
        dispatch!(self, s => s.execution_exists(id))
    }

    /// Atomically append events and update the execution projection.
    pub fn append(&mut self, state: &ExecutionState, events: &[HistoryEvent]) -> Result<()> {
        dispatch!(self, s => s.append(state, events))
    }

    pub fn history(&self, id: &str) -> Result<Vec<HistoryEvent>> {
        dispatch!(self, s => s.history(id))
    }

    /// Ids of executions that have not reached a terminal status.
    pub fn active_execution_ids(&self) -> Result<Vec<String>> {
        dispatch!(self, s => s.active_execution_ids())
    }

    pub fn list_executions(
        &self,
        status: Option<&str>,
        workflow: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ExecutionSummary>> {
        dispatch!(self, s => s.list_executions(status, workflow, limit))
    }

    pub fn count_by_status(&self) -> Result<Vec<(String, u64)>> {
        dispatch!(self, s => s.count_by_status())
    }

    pub fn upsert_worker(&self, w: &WorkerRecord) -> Result<()> {
        dispatch!(self, s => s.upsert_worker(w))
    }

    pub fn touch_worker(&self, worker_id: &str, now: u64) -> Result<()> {
        dispatch!(self, s => s.touch_worker(worker_id, now))
    }

    pub fn workers(&self) -> Result<Vec<WorkerRecord>> {
        dispatch!(self, s => s.workers())
    }
}
