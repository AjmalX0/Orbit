//! The ORBIT runtime: a durable, single-node workflow execution service.
//!
//! * [`model`]  — history events and the state folded from them
//! * [`decide`] — the pure, deterministic workflow decider
//! * [`store`]  — SQLite persistence (event history is authoritative)
//! * [`engine`] — dispatch, worker registry, reconciliation and recovery
//! * [`server`] — HTTP API and the HTTP/JSON worker protocol binding
//! * [`cluster`] — active/standby operation of several servers on PostgreSQL
//! * [`shared`] — several active servers sharing the work on PostgreSQL
//! * [`testing`] — in-process simulation for `orbit test` and `orbit simulate`
//! * [`ui`] — the built-in web UI served at `/ui/`
//! * [`replay`] — re-run a recorded history through the current decider
//! * [`schedule`] — cron and interval schedules that start executions

// TaskError is returned by value on cold paths; boxing would only add noise.
#![allow(clippy::result_large_err)]

pub mod cluster;
pub mod decide;
pub mod engine;
pub mod grpc;
pub mod http_adapter;
pub mod identity;
pub mod metrics;
pub mod model;
pub mod replay;
pub mod schedule;
pub mod server;
pub mod shared;
pub mod store;
pub mod telemetry;
pub mod testing;
pub mod ui;

pub use engine::{Engine, EngineConfig, EngineError};
pub use store::Store;
