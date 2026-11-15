//! Persistence.
//!
//! The `execution_events` table is the source of truth. `workflow_executions`
//! is a projection kept in the same transaction for fast listing. Every batch
//! of events is appended atomically together with the projection update.
//! `execution_events` holds each execution's current run; runs that ended
//! with continue-as-new are moved to `execution_run_events` (keyed by run) in
//! the transaction that starts the next run. `orbit_schedules` holds the
//! schedules (see `crate::schedule`).
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

pub(crate) use self::postgres::{session_client, CLUSTER_SCHEMA};
pub use self::postgres::{PgMode, PgStore};
pub(crate) use self::postgres::{SharedAppend, TaskOffer, UnitSync, NOW_MS, SHARED_SCHEMA};
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
CREATE TABLE IF NOT EXISTS execution_run_events (
    execution_id TEXT NOT NULL,
    run BIGINT NOT NULL,
    seq BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    step TEXT,
    payload TEXT NOT NULL,
    timestamp_ms BIGINT NOT NULL,
    PRIMARY KEY (execution_id, run, seq)
);
CREATE TABLE IF NOT EXISTS orbit_schedules (
    schedule_id TEXT PRIMARY KEY,
    spec TEXT NOT NULL,
    paused BIGINT NOT NULL,
    next_fire_ms BIGINT,
    due_ms BIGINT,
    last_fire_ms BIGINT,
    last_execution_id TEXT,
    fired_count BIGINT NOT NULL,
    skipped_count BIGINT NOT NULL,
    last_error TEXT,
    claimed_by TEXT,
    claimed_until_ms BIGINT,
    created_at_ms BIGINT NOT NULL,
    updated_at_ms BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_orbit_schedules_due ON orbit_schedules(due_ms);
"#;

/// A schedule as stored in `orbit_schedules`. `spec` is the JSON of
/// [`crate::schedule::ScheduleSpec`]; the other columns are the schedule's
/// progress, written by whichever server fires it.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleRow {
    pub schedule_id: String,
    pub spec: String,
    pub paused: bool,
    /// The next occurrence (nominal time); `None` once the schedule ended.
    pub next_fire_ms: Option<u64>,
    /// When that occurrence fires: `next_fire_ms` plus the jitter.
    pub due_ms: Option<u64>,
    pub last_fire_ms: Option<u64>,
    pub last_execution_id: Option<String>,
    pub fired: u64,
    pub skipped: u64,
    pub last_error: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// The outcome of firing one occurrence, written when the firing server
/// releases its claim (see [`Store::finish_schedule_fire`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FireRecord {
    pub next_fire_ms: Option<u64>,
    pub due_ms: Option<u64>,
    /// Set when an occurrence was handled (started or skipped).
    pub last_fire_ms: Option<u64>,
    /// Set when an execution was started.
    pub last_execution_id: Option<String>,
    pub fired: u64,
    pub skipped: u64,
    pub last_error: Option<String>,
}

/// Continue-as-new: the finished run's number and its last events. They are
/// archived, together with the rest of that run's history, in the
/// transaction that writes the next run's first events.
pub struct Rollover<'a> {
    pub run: u32,
    pub events: &'a [HistoryEvent],
}

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

    /// Continue-as-new in one transaction: archive the finished run (its
    /// stored history plus `rollover.events`) under its run number, then
    /// write the next run's first `events` and point the projection at it.
    pub fn append_rollover(
        &mut self,
        state: &ExecutionState,
        events: &[HistoryEvent],
        rollover: Rollover<'_>,
    ) -> Result<()> {
        dispatch!(self, s => s.append_rollover(state, events, rollover))
    }

    /// The history of an earlier (archived) run; empty if there is none.
    pub fn run_history(&self, id: &str, run: u32) -> Result<Vec<HistoryEvent>> {
        dispatch!(self, s => s.run_history(id, run))
    }

    /// The projected status of an execution (its latest run), if it exists.
    pub fn execution_status(&self, id: &str) -> Result<Option<String>> {
        dispatch!(self, s => s.execution_status(id))
    }

    /// True when this store cannot write (a cluster standby).
    pub fn read_only(&self) -> bool {
        matches!(self, Store::Postgres(p) if p.mode() == PgMode::ReadOnly)
    }

    /// True for a shared-mode store (several servers write).
    pub fn shared(&self) -> bool {
        matches!(self, Store::Postgres(p) if p.mode() == PgMode::Shared)
    }

    /// Drop every claim on a schedule occurrence. Only for a server that is
    /// the only writer (single server, cluster leader): claims left behind
    /// are from a process that is gone.
    pub fn release_schedule_claims(&self) -> Result<u64> {
        dispatch!(self, s => s.release_schedule_claims())
    }

    // -------------------------------------------------------------- schedules

    /// Insert a new schedule; false if the id is taken.
    pub fn insert_schedule(&self, row: &ScheduleRow) -> Result<bool> {
        dispatch!(self, s => s.insert_schedule(row))
    }

    /// Overwrite a schedule's spec, paused flag and next occurrence, and drop
    /// any claim on it; false if it does not exist.
    pub fn update_schedule(&self, row: &ScheduleRow) -> Result<bool> {
        dispatch!(self, s => s.update_schedule(row))
    }

    pub fn schedule(&self, id: &str) -> Result<Option<ScheduleRow>> {
        dispatch!(self, s => s.schedule(id))
    }

    pub fn schedules(&self) -> Result<Vec<ScheduleRow>> {
        dispatch!(self, s => s.schedules())
    }

    pub fn delete_schedule(&self, id: &str) -> Result<bool> {
        dispatch!(self, s => s.delete_schedule(id))
    }

    /// Unpaused schedules whose next occurrence is due at `now` and that no
    /// server holds a live claim on.
    pub fn due_schedules(&self, now: u64) -> Result<Vec<ScheduleRow>> {
        dispatch!(self, s => s.due_schedules(now))
    }

    /// Claim the occurrence `next_fire_ms` of a schedule for `node` until
    /// `until`: a compare-and-set on the row, so of several servers that
    /// find the same occurrence due exactly one gets it.
    pub fn claim_schedule(
        &self,
        id: &str,
        next_fire_ms: u64,
        node: &str,
        now: u64,
        until: u64,
    ) -> Result<bool> {
        dispatch!(self, s => s.claim_schedule(id, next_fire_ms, node, now, until))
    }

    /// Record a handled occurrence and release the claim, if `node` still
    /// holds it on occurrence `next_fire_ms`. False when the row changed
    /// meanwhile (updated, paused, deleted or claimed by another server).
    pub fn finish_schedule_fire(
        &self,
        id: &str,
        next_fire_ms: u64,
        node: &str,
        now: u64,
        record: &FireRecord,
    ) -> Result<bool> {
        dispatch!(self, s => s.finish_schedule_fire(id, next_fire_ms, node, now, record))
    }

    /// Give up a claim without advancing the schedule: the occurrence is
    /// tried again from `retry_at` by any server.
    pub fn release_schedule_claim(
        &self,
        id: &str,
        node: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<()> {
        dispatch!(self, s => s.release_schedule_claim(id, node, retry_at, error, now))
    }

    /// Note an execution started by `POST /v1/schedules/{id}/trigger`.
    pub fn record_schedule_trigger(&self, id: &str, execution_id: &str, now: u64) -> Result<()> {
        dispatch!(self, s => s.record_schedule_trigger(id, execution_id, now))
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
