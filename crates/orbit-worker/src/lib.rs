//! Rust worker SDK for the ORBIT orchestration runtime.
//!
//! ```no_run
//! use orbit_worker::{TaskError, Worker};
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Deserialize)]
//! struct Input { order_id: String }
//! #[derive(Serialize)]
//! struct Output { reservation_id: String }
//!
//! let mut worker = Worker::builder("inventory-rust").build();
//! worker.task("inventory.reserve", "1.0.0", |input: Input, ctx| {
//!     if ctx.is_cancelled() {
//!         return Err(TaskError::permanent("CANCELLED", "stopped"));
//!     }
//!     Ok(Output { reservation_id: format!("res-{}", input.order_id) })
//! });
//! worker.run().unwrap();
//! ```
//!
//! Delivery is at-least-once: a task can be delivered again after a crash
//! ([`Context::redelivery`]). Use [`Context::idempotency_key`], which is stable
//! across retries and redeliveries, to make side effects idempotent.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A structured task failure returned from a handler.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    /// Boxed to keep `Result<_, TaskError>` small (clippy `result_large_err`).
    pub details: Option<Box<Value>>,
}

impl TaskError {
    /// An error the runtime may retry according to the step's retry policy.
    pub fn retryable(code: &str, message: impl Into<String>) -> Self {
        TaskError {
            code: code.into(),
            message: message.into(),
            retryable: true,
            details: None,
        }
    }

    /// An error that fails the step immediately.
    pub fn permanent(code: &str, message: impl Into<String>) -> Self {
        TaskError {
            code: code.into(),
            message: message.into(),
            retryable: false,
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(Box::new(details));
        self
    }
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for TaskError {}

/// Error talking to the runtime (`status == 0` for connection failures).
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.code, self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

/// One delivery of a task (protocol `ExecuteTaskRequest`).
#[derive(Debug, Clone, Deserialize)]
pub struct Assignment {
    pub token: String,
    pub execution_id: String,
    #[serde(default)]
    pub workflow: String,
    #[serde(default)]
    pub workflow_version: String,
    pub step_id: String,
    pub task_name: String,
    #[serde(default)]
    pub contract_version: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub idempotency_key: String,
    #[serde(default = "one")]
    pub attempt: u32,
    #[serde(default)]
    pub redelivery: bool,
    #[serde(default)]
    pub deadline_ms: Option<u64>,
}

fn one() -> u32 {
    1
}

/// Information about the attempt being executed.
pub struct Context {
    pub assignment: Assignment,
    cancelled: Arc<AtomicBool>,
}

impl Context {
    pub fn execution_id(&self) -> &str {
        &self.assignment.execution_id
    }
    pub fn step_id(&self) -> &str {
        &self.assignment.step_id
    }
    pub fn attempt(&self) -> u32 {
        self.assignment.attempt
    }
    pub fn idempotency_key(&self) -> &str {
        &self.assignment.idempotency_key
    }
    pub fn redelivery(&self) -> bool {
        self.assignment.redelivery
    }
    /// True once the runtime asked this attempt to stop (timeout or cancellation).
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    /// Milliseconds left before the attempt's deadline, if it has one.
    pub fn remaining_ms(&self) -> Option<u64> {
        self.assignment
            .deadline_ms
            .map(|d| d.saturating_sub(now_ms()))
    }
    pub fn log(&self, message: &str) {
        eprintln!(
            "{}",
            json!({ "level": "info", "message": message, "execution_id": self.execution_id(), "step": self.step_id(), "attempt": self.attempt() })
        );
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

type Handler = Box<dyn Fn(Value, &Context) -> Result<Value, TaskError> + Send + Sync>;

struct Registered {
    version: String,
    handler: Handler,
}

pub struct WorkerBuilder {
    worker_id: String,
    server: String,
    token: Option<String>,
    ca_pem: Option<Vec<u8>>,
    concurrency: usize,
    runtime: String,
    poll_wait_ms: u64,
}

impl WorkerBuilder {
    pub fn server(mut self, url: &str) -> Self {
        self.server = url.trim_end_matches('/').to_string();
        self
    }
    pub fn token(mut self, token: &str) -> Self {
        self.token = Some(token.to_string());
        self
    }
    /// Trust an extra PEM CA certificate (runtime with a private certificate).
    pub fn ca_pem(mut self, pem: Vec<u8>) -> Self {
        self.ca_pem = Some(pem);
        self
    }
    pub fn concurrency(mut self, n: usize) -> Self {
        self.concurrency = n.max(1);
        self
    }
    pub fn runtime(mut self, runtime: &str) -> Self {
        self.runtime = runtime.to_string();
        self
    }
    pub fn poll_wait(mut self, d: Duration) -> Self {
        self.poll_wait_ms = d.as_millis() as u64;
        self
    }

    pub fn build(self) -> Worker {
        let mut config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_millis(self.poll_wait_ms + 15_000)));
        if let Some(pem) = &self.ca_pem {
            use ureq::tls::{PemItem, RootCerts, TlsConfig};
            let certs: Vec<_> = ureq::tls::parse_pem(pem)
                .filter_map(|i| match i {
                    Ok(PemItem::Certificate(c)) => Some(c),
                    _ => None,
                })
                .collect();
            config = config.tls_config(
                TlsConfig::builder()
                    .root_certs(RootCerts::new_with_certs(&certs))
                    .build(),
            );
        }
        Worker {
            inner: Arc::new(Inner {
                agent: config.build().into(),
                worker_id: self.worker_id,
                server: self.server,
                token: self.token,
                concurrency: self.concurrency,
                runtime: self.runtime,
                poll_wait_ms: self.poll_wait_ms,
                tasks: Mutex::new(HashMap::new()),
                session: Mutex::new(None),
                heartbeat_ms: Mutex::new(5_000),
                active: Mutex::new(HashMap::new()),
                stop: AtomicBool::new(false),
            }),
        }
    }
}

struct Inner {
    agent: ureq::Agent,
    worker_id: String,
    server: String,
    token: Option<String>,
    concurrency: usize,
    runtime: String,
    poll_wait_ms: u64,
    tasks: Mutex<HashMap<String, Arc<Registered>>>,
    session: Mutex<Option<String>>,
    heartbeat_ms: Mutex<u64>,
    active: Mutex<HashMap<String, Arc<AtomicBool>>>,
    stop: AtomicBool,
}

/// Runs task handlers for the ORBIT runtime. Cheap to clone; clones share state,
/// so a clone can call [`Worker::stop`] from another thread.
#[derive(Clone)]
pub struct Worker {
    inner: Arc<Inner>,
}

impl Worker {
    /// Defaults come from `ORBIT_SERVER`, `ORBIT_WORKER_TOKEN` / `ORBIT_TOKEN` and `ORBIT_CA_CERT`.
    pub fn builder(worker_id: &str) -> WorkerBuilder {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        WorkerBuilder {
            worker_id: worker_id.to_string(),
            server: env("ORBIT_SERVER")
                .unwrap_or_else(|| "http://127.0.0.1:7233".into())
                .trim_end_matches('/')
                .to_string(),
            token: env("ORBIT_WORKER_TOKEN").or_else(|| env("ORBIT_TOKEN")),
            ca_pem: env("ORBIT_CA_CERT").and_then(|p| std::fs::read(p).ok()),
            concurrency: 4,
            runtime: "rust".into(),
            poll_wait_ms: 20_000,
        }
    }

    /// Register a typed handler for `name@version`. Input is deserialized from
    /// the task payload; a deserialization failure fails the attempt permanently.
    pub fn task<I, O, F>(&mut self, name: &str, version: &str, handler: F) -> &mut Self
    where
        I: DeserializeOwned,
        O: Serialize,
        F: Fn(I, &Context) -> Result<O, TaskError> + Send + Sync + 'static,
    {
        let wrapped: Handler = Box::new(move |input, ctx| {
            let typed: I = serde_json::from_value(input)
                .map_err(|e| TaskError::permanent("INVALID_INPUT", e.to_string()))?;
            let out = handler(typed, ctx)?;
            serde_json::to_value(out)
                .map_err(|e| TaskError::permanent("INVALID_RESULT", e.to_string()))
        });
        {
            let mut tasks = self.inner.tasks.lock().unwrap();
            assert!(!tasks.contains_key(name), "task {name} registered twice");
            tasks.insert(
                name.to_string(),
                Arc::new(Registered {
                    version: version.to_string(),
                    handler: wrapped,
                }),
            );
        }
        self
    }

    /// Ask [`Worker::run`] to return after in-flight tasks finish.
    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
    }

    /// Poll for work until [`Worker::stop`] is called.
    pub fn run(&self) -> Result<(), ApiError> {
        if self.inner.tasks.lock().unwrap().is_empty() {
            return Err(ApiError {
                status: 0,
                code: "NO_TASKS".into(),
                message: "no tasks registered".into(),
            });
        }
        let inner = &self.inner;
        inner.retrying(|| inner.register().map(|_| ()))?;
        std::thread::scope(|s| {
            s.spawn(|| inner.heartbeat_loop());
            for _ in 0..inner.concurrency {
                s.spawn(|| inner.poll_loop());
            }
        });
        if let Some(session) = inner.session.lock().unwrap().clone() {
            let _ = inner.call(
                "/v1/workers/unregister",
                &json!({ "worker_session_id": session }),
            );
        }
        eprintln!("[orbit] worker {} stopped", inner.worker_id);
        Ok(())
    }
}

impl Inner {
    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn call(&self, path: &str, body: &Value) -> Result<(u16, Value), ApiError> {
        let mut req = self.agent.post(format!("{}{path}", self.server));
        if let Some(t) = &self.token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let mut resp = req.send_json(body).map_err(|e| ApiError {
            status: 0,
            code: "CONNECTION".into(),
            message: format!("cannot reach ORBIT runtime at {}: {e}", self.server),
        })?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        let value: Value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        if status >= 400 {
            return Err(ApiError {
                status,
                code: value["error"]["code"]
                    .as_str()
                    .unwrap_or("HTTP_ERROR")
                    .to_string(),
                message: value["error"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        Ok((status, value))
    }

    fn register(&self) -> Result<String, ApiError> {
        let contracts: Vec<Value> = self
            .tasks
            .lock()
            .unwrap()
            .iter()
            .map(|(n, t)| json!({ "task_name": n, "contract_version": t.version }))
            .collect();
        let count = contracts.len();
        let (_, r) = self.call(
            "/v1/workers/register",
            &json!({ "worker_id": self.worker_id, "runtime": self.runtime, "sdk": format!("orbit-rust/{VERSION}"), "contracts": contracts }),
        )?;
        let session = r["worker_session_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        *self.session.lock().unwrap() = Some(session.clone());
        if let Some(ms) = r["heartbeat_interval_ms"].as_u64() {
            *self.heartbeat_ms.lock().unwrap() = ms.max(500);
        }
        for w in r["warnings"].as_array().into_iter().flatten() {
            eprintln!("[orbit] runtime: {}", w.as_str().unwrap_or_default());
        }
        eprintln!(
            "[orbit] worker {} registered ({count} tasks)",
            self.worker_id
        );
        Ok(session)
    }

    /// Run `f(session)`, re-registering once if the runtime forgot the session.
    fn with_session(
        &self,
        f: impl Fn(&str) -> Result<(u16, Value), ApiError>,
    ) -> Result<(u16, Value), ApiError> {
        let current = self.session.lock().unwrap().clone();
        let session = match current {
            Some(s) => s,
            None => self.register()?,
        };
        match f(&session) {
            Err(e) if e.status == 404 && e.code == "UNKNOWN_SESSION" => f(&self.register()?),
            other => other,
        }
    }

    fn retrying<T>(&self, f: impl Fn() -> Result<T, ApiError>) -> Result<T, ApiError> {
        let mut delay = 200u64;
        loop {
            match f() {
                Err(e) if e.status == 0 || e.status >= 500 => {
                    if self.stopping() {
                        eprintln!("[orbit] {e}; giving up because the worker is stopping");
                        return Err(e);
                    }
                    eprintln!("[orbit] {e}; retrying in {delay}ms");
                    std::thread::sleep(Duration::from_millis(delay));
                    delay = (delay * 2).min(5_000);
                }
                other => return other,
            }
        }
    }

    fn sleep_unless_stopped(&self, ms: u64) -> bool {
        let mut left = ms;
        while left > 0 {
            if self.stopping() {
                return false;
            }
            let step = left.min(50);
            std::thread::sleep(Duration::from_millis(step));
            left -= step;
        }
        !self.stopping()
    }

    fn heartbeat_loop(&self) {
        loop {
            let every = *self.heartbeat_ms.lock().unwrap();
            if !self.sleep_unless_stopped(every) {
                return;
            }
            if let Ok((_, r)) = self.with_session(|s| {
                self.call("/v1/workers/heartbeat", &json!({ "worker_session_id": s }))
            }) {
                let active = self.active.lock().unwrap();
                for token in r["cancel"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    if let Some(flag) = active.get(token) {
                        flag.store(true, Ordering::SeqCst);
                    }
                }
            }
        }
    }

    fn poll_loop(&self) {
        let mut backoff = 200u64;
        while !self.stopping() {
            let polled = self.with_session(|s| {
                self.call(
                    "/v1/workers/poll",
                    &json!({ "worker_session_id": s, "wait_ms": self.poll_wait_ms }),
                )
            });
            let (status, body) = match polled {
                Ok(r) => {
                    backoff = 200;
                    r
                }
                Err(e) => {
                    if self.stopping() {
                        return;
                    }
                    eprintln!("[orbit] poll failed: {e}; retrying in {backoff}ms");
                    self.sleep_unless_stopped(backoff);
                    backoff = (backoff * 2).min(5_000);
                    continue;
                }
            };
            if status == 204 || body.is_null() {
                continue;
            }
            match serde_json::from_value::<Assignment>(body) {
                Ok(a) => self.execute(a),
                Err(e) => eprintln!("[orbit] malformed task assignment: {e}"),
            }
        }
    }

    fn execute(&self, assignment: Assignment) {
        let cancelled = Arc::new(AtomicBool::new(false));
        let token = assignment.token.clone();
        self.active
            .lock()
            .unwrap()
            .insert(token.clone(), cancelled.clone());
        let registered = self
            .tasks
            .lock()
            .unwrap()
            .get(&assignment.task_name)
            .cloned();
        let input = assignment.input.clone();
        let ctx = Context {
            assignment,
            cancelled,
        };
        let result = match registered {
            None => Err(TaskError::retryable(
                "UNKNOWN_TASK",
                format!("worker does not implement {}", ctx.assignment.task_name),
            )),
            Some(r) => {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (r.handler)(input, &ctx)))
                    .unwrap_or_else(|p| {
                        let msg = p
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| p.downcast_ref::<String>().cloned())
                            .unwrap_or_default();
                        Err(TaskError::retryable("PANIC", msg))
                    })
            }
        };
        self.active.lock().unwrap().remove(&token);
        let (path, body) = match result {
            Ok(mut output) => {
                if output.is_null() {
                    output = json!({});
                }
                if output.is_object() {
                    (
                        "/v1/tasks/complete",
                        json!({ "token": token, "output": output }),
                    )
                } else {
                    let e = TaskError::permanent("INVALID_RESULT", "tasks must return an object");
                    (
                        "/v1/tasks/fail",
                        json!({ "token": token, "error_code": e.code, "error_message": e.message, "retryable": false }),
                    )
                }
            }
            Err(e) => (
                "/v1/tasks/fail",
                json!({ "token": token, "error_code": e.code, "error_message": e.message, "retryable": e.retryable, "details": e.details }),
            ),
        };
        // Retry through outages so a result survives a runtime restart.
        match self.retrying(|| self.call(path, &body)) {
            Ok((_, ack)) if ack["accepted"] == json!(false) => {
                eprintln!(
                    "[orbit] runtime ignored result: {}",
                    ack["reason"].as_str().unwrap_or_default()
                )
            }
            Ok(_) => {}
            Err(e) => eprintln!("[orbit] could not deliver result: {e}"),
        }
    }
}
