//! The ORBIT runtime: a durable, single-node workflow execution service.
//!
//! * [`model`]  — history events and the state folded from them
//! * [`decide`] — the pure, deterministic workflow decider
//! * [`store`]  — SQLite persistence (event history is authoritative)
//! * [`engine`] — dispatch, worker registry, reconciliation and recovery
//! * [`server`] — HTTP API and the HTTP/JSON worker protocol binding
//! * [`cluster`] — active/standby operation of several servers on PostgreSQL

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
pub mod server;
pub mod store;
pub mod telemetry;

pub use engine::{Engine, EngineConfig, EngineError};
pub use store::Store;
