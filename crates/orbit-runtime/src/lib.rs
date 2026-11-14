//! The ORBIT runtime: a durable, single-node workflow execution service.
//!
//! * [`model`]  — history events and the state folded from them
//! * [`decide`] — the pure, deterministic workflow decider
//! * [`store`]  — SQLite persistence (event history is authoritative)
//! * [`engine`] — dispatch, worker registry, reconciliation and recovery
//! * [`server`] — HTTP API and the HTTP/JSON worker protocol binding

pub mod decide;
pub mod engine;
pub mod http_adapter;
pub mod metrics;
pub mod model;
pub mod server;
pub mod store;

pub use engine::{Engine, EngineConfig, EngineError};
pub use store::Store;
