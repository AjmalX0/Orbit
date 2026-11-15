//! HTTP API (RFC-0001 §11.2) and the HTTP/JSON binding of the worker
//! protocol (§8). See `protocol/README.md` for the wire format.

use crate::engine::{Engine, EngineError, RegisterWorker, TaskAssignment};
use crate::identity::{Identities, WorkerIdentity};
use crate::model::TaskError;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

pub const API_VERSION: &str = "orbit-api/1";

pub struct AppState {
    pub engine: Mutex<Engine>,
    pub changed: Arc<Notify>,
    pub auth_token: Option<String>,
    /// Optional credential restricted to the worker protocol.
    pub worker_token: Option<String>,
    /// Per-worker identities (`--workers-file`).
    pub identities: Identities,
}

pub type Shared = Arc<AppState>;

pub struct ApiError(pub(crate) EngineError);

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (
            status,
            Json(json!({ "error": { "code": self.0.code, "message": self.0.message } })),
        )
            .into_response()
    }
}

fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError(EngineError::new(400, "INVALID_REQUEST", msg))
}

type ApiResult<T> = Result<T, ApiError>;

pub(crate) fn engine(state: &Shared) -> std::sync::MutexGuard<'_, Engine> {
    state.engine.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn router(state: Shared, body_limit: usize) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/metrics", get(metrics))
        .route("/v1/workflows", post(register_workflow).get(list_workflows))
        .route("/v1/workflows/{name}", get(get_workflow_latest))
        .route("/v1/workflows/{name}/{version}", get(get_workflow))
        .route("/v1/executions", post(start_execution).get(list_executions))
        .route("/v1/executions/{id}", get(get_execution))
        .route("/v1/executions/{id}/history", get(get_history))
        .route("/v1/executions/{id}/cancel", post(cancel_execution))
        .route("/v1/executions/{id}/signals/{name}", post(send_signal))
        .route("/v1/executions/{id}/queries/{name}", get(run_query))
        .route("/v1/executions/{id}/wait", get(wait_execution))
        .route("/v1/workers", get(list_workers))
        .route("/v1/workers/register", post(register_worker))
        .route("/v1/workers/heartbeat", post(heartbeat))
        .route("/v1/workers/poll", post(poll))
        .route("/v1/workers/unregister", post(unregister))
        .route("/v1/tasks/complete", post(complete_task))
        .route("/v1/tasks/fail", post(fail_task))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .layer(DefaultBodyLimit::max(body_limit))
        .with_state(state)
}

/// Endpoints a worker credential may call.
fn is_worker_path(path: &str) -> bool {
    path.starts_with("/v1/workers/") || path.starts_with("/v1/tasks/")
}

/// Who is calling, as established by [`auth`].
#[derive(Clone)]
pub enum Caller {
    /// No authentication is configured (loopback development).
    Open,
    Admin,
    /// The shared, restricted worker token (`--worker-token`).
    SharedWorker,
    /// A worker from the workers file, by token or client certificate.
    Worker(Arc<WorkerIdentity>),
    /// An application from the workers file (`[[client]]`): the execution
    /// API for the workflows and actions it was granted.
    Client(Arc<crate::identity::ClientIdentity>),
}

impl Caller {
    fn identity(&self) -> Option<&WorkerIdentity> {
        match self {
            Caller::Worker(w) => Some(w),
            _ => None,
        }
    }
}

fn forbidden(msg: impl Into<String>) -> ApiError {
    ApiError(EngineError::new(403, "FORBIDDEN", msg))
}

/// A client identity may only act on the workflows and actions it was granted.
fn allow(caller: &Caller, action: &str, workflow: &str) -> ApiResult<()> {
    if let Caller::Client(c) = caller {
        if !c.allows(action, workflow) {
            return Err(forbidden(format!(
                "client `{}` may not {action} executions of `{workflow}`",
                c.id
            )));
        }
    }
    Ok(())
}

/// `allow` for an existing execution, looked up by id.
fn allow_execution(state: &Shared, caller: &Caller, action: &str, id: &str) -> ApiResult<()> {
    if matches!(caller, Caller::Client(_)) {
        let workflow = engine(state).execution(id)?.workflow;
        allow(caller, action, &workflow)?;
    }
    Ok(())
}

/// Authentication.
///
/// When any credential is configured, every endpoint except `/v1/health`
/// requires one. The admin token can call anything. Worker credentials (the
/// shared `--worker-token`, a per-worker token from the workers file, or a
/// client certificate under mutual TLS) can only call the worker protocol
/// endpoints, so a compromised worker host cannot start, cancel or read
/// executions. Per-worker identities are further limited by the handlers to
/// their own id, tasks, sessions and task tokens.
/// Establish the caller from a bearer token and/or a verified client
/// certificate. Shared by the HTTP API and the gRPC binding. `Ok(None)` means
/// no credential was presented while authentication is required.
pub(crate) fn identify(
    state: &AppState,
    bearer: Option<&str>,
    cert_der: Option<&[u8]>,
) -> Result<Option<Caller>, ApiError> {
    let secured = state.auth_token.is_some() || !state.identities.is_empty();
    if !secured {
        return Ok(Some(Caller::Open));
    }
    if let Some(t) = bearer {
        if state
            .auth_token
            .as_ref()
            .is_some_and(|a| constant_time_eq(t.as_bytes(), a.as_bytes()))
        {
            return Ok(Some(Caller::Admin));
        }
        if let Some(w) = state.identities.by_token(t) {
            return Ok(Some(Caller::Worker(Arc::new(w.clone()))));
        }
        if let Some(c) = state.identities.client_by_token(t) {
            return Ok(Some(Caller::Client(Arc::new(c.clone()))));
        }
        if state
            .worker_token
            .as_ref()
            .is_some_and(|w| constant_time_eq(t.as_bytes(), w.as_bytes()))
        {
            return Ok(Some(Caller::SharedWorker));
        }
    }
    if let Some(der) = cert_der {
        if let Some(w) = state.identities.by_certificate(der) {
            return Ok(Some(Caller::Worker(Arc::new(w.clone()))));
        }
        return match state.identities.client_by_certificate(der) {
            Some(c) => Ok(Some(Caller::Client(Arc::new(c.clone())))),
            None => Err(forbidden(
                "the client certificate is valid but matches no worker or client in the workers file",
            )),
        };
    }
    Ok(None)
}

async fn auth(State(state): State<Shared>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let cert = req
        .extensions()
        .get::<Arc<axum_server::tls::TlsInfo>>()
        .and_then(|info| info.peer_certificates.first().cloned());
    let caller = match identify(&state, bearer.as_deref(), cert.as_deref()) {
        Ok(Some(c)) => c,
        Ok(None) if path == "/v1/health" => Caller::Open,
        Ok(None) => {
            return ApiError(EngineError::new(
                401,
                "UNAUTHENTICATED",
                "missing or invalid credentials",
            ))
            .into_response()
        }
        Err(e) => return e.into_response(),
    };
    if matches!(caller, Caller::SharedWorker | Caller::Worker(_))
        && !is_worker_path(&path)
        && path != "/v1/health"
    {
        return forbidden("worker credentials may only call /v1/workers/* and /v1/tasks/*")
            .into_response();
    }
    if let Caller::Client(c) = &caller {
        let admin_only = (path == "/v1/workflows" && req.method() == axum::http::Method::POST)
            || path.starts_with("/v1/workers")
            || path == "/v1/metrics";
        if is_worker_path(&path) || admin_only {
            return forbidden(format!(
                "client `{}` may only use the execution API (start, signal, query, read, cancel)",
                c.id
            ))
            .into_response();
        }
    }
    req.extensions_mut().insert(caller);
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .insert("x-orbit-api-version", HeaderValue::from_static(API_VERSION));
    resp
}

/// A per-worker identity may only use sessions it opened.
pub(crate) fn authorize_session(state: &Shared, caller: &Caller, session: &str) -> ApiResult<()> {
    if let Some(id) = caller.identity() {
        if let Some(owner) = engine(state).session_worker(session) {
            if owner != id.id {
                return Err(forbidden(format!(
                    "session belongs to worker `{owner}`, not `{}`",
                    id.id
                )));
            }
        }
    }
    Ok(())
}

/// A per-worker identity may only report results for tokens issued to it.
pub(crate) fn authorize_token(state: &Shared, caller: &Caller, token: &str) -> ApiResult<()> {
    if let Some(id) = caller.identity() {
        if let Some(owner) = engine(state).token_worker(token) {
            if owner != id.id {
                return Err(forbidden(format!(
                    "task token was issued to worker `{owner}`, not `{}`",
                    id.id
                )));
            }
        }
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn health(State(state): State<Shared>) -> Json<Value> {
    let e = engine(&state);
    Json(json!({
        "status": "ok",
        "api_version": API_VERSION,
        "runtime_version": env!("CARGO_PKG_VERSION"),
        "ir_versions": orbit_ir::SUPPORTED_IR_VERSIONS,
        "active_executions": e.active_count(),
        "worker_sessions": e.session_count(),
    }))
}

async fn metrics(State(state): State<Shared>) -> Response {
    let e = engine(&state);
    let body = e.metrics.render(e.active_count(), e.session_count());
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    (headers, body).into_response()
}

async fn register_workflow(State(state): State<Shared>, body: String) -> ApiResult<Response> {
    let ir = orbit_ir::WorkflowIr::from_json(&body)
        .map_err(|e| ApiError(EngineError::new(400, "INVALID_IR", e.0)))?;
    let (name, version, hash) = (
        ir.name.clone(),
        ir.version.clone(),
        ir.definition_hash.clone(),
    );
    let created = engine(&state).register_definition(ir)?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(json!({ "name": name, "version": version, "definition_hash": hash, "created": created }))).into_response())
}

fn summary(d: &orbit_ir::WorkflowIr) -> Value {
    json!({
        "name": d.name,
        "version": d.version,
        "description": d.description,
        "definition_hash": d.definition_hash,
        "steps": d.steps.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
        "tasks": d.contracts.keys().collect::<Vec<_>>(),
    })
}

async fn list_workflows(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
) -> Json<Value> {
    let defs = engine(&state).definitions();
    let visible = |name: &str| match &caller {
        Caller::Client(c) => c.can_read(name),
        _ => true,
    };
    Json(
        json!({ "workflows": defs.iter().filter(|d| visible(&d.name)).map(|d| summary(d)).collect::<Vec<_>>() }),
    )
}

async fn get_workflow(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path((name, version)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    allow(&caller, "read", &name)?;
    let d = engine(&state)
        .definition(&name, Some(&version))
        .ok_or_else(|| {
            ApiError(EngineError::new(
                404,
                "WORKFLOW_NOT_FOUND",
                "workflow not found",
            ))
        })?;
    Ok(Json(serde_json::to_value(&*d).unwrap_or_default()))
}

async fn get_workflow_latest(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    allow(&caller, "read", &name)?;
    let d = engine(&state).definition(&name, None).ok_or_else(|| {
        ApiError(EngineError::new(
            404,
            "WORKFLOW_NOT_FOUND",
            "workflow not found",
        ))
    })?;
    Ok(Json(serde_json::to_value(&*d).unwrap_or_default()))
}

#[derive(Deserialize)]
struct StartRequest {
    workflow: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    input: Value,
    #[serde(default)]
    execution_id: Option<String>,
}

async fn start_execution(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Json(req): Json<StartRequest>,
) -> ApiResult<Response> {
    allow(&caller, "start", &req.workflow)?;
    let r = engine(&state).start(
        &req.workflow,
        req.version.as_deref(),
        req.input,
        req.execution_id,
    )?;
    let status = if r.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(serde_json::to_value(r).unwrap_or_default())).into_response())
}

#[derive(Deserialize)]
struct ListQuery {
    status: Option<String>,
    workflow: Option<String>,
    limit: Option<u32>,
}

async fn list_executions(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let mut list = engine(&state).list_executions(
        q.status.as_deref().map(str::to_uppercase).as_deref(),
        q.workflow.as_deref(),
        q.limit.unwrap_or(50),
    )?;
    if let Caller::Client(c) = &caller {
        list.retain(|e| c.can_read(&e.workflow));
    }
    Ok(Json(json!({ "executions": list })))
}

async fn get_execution(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    allow_execution(&state, &caller, "read", &id)?;
    let s = engine(&state).execution(&id)?;
    Ok(Json(serde_json::to_value(s).unwrap_or_default()))
}

async fn get_history(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    allow_execution(&state, &caller, "read", &id)?;
    let h = engine(&state).history(&id)?;
    Ok(Json(json!({ "execution_id": id, "events": h })))
}

#[derive(Deserialize, Default)]
struct CancelRequest {
    #[serde(default)]
    reason: Option<String>,
}

async fn cancel_execution(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    body: Option<Json<CancelRequest>>,
) -> ApiResult<Json<Value>> {
    allow_execution(&state, &caller, "cancel", &id)?;
    let reason = body
        .and_then(|b| b.0.reason)
        .unwrap_or_else(|| "requested via API".into());
    let s = engine(&state).cancel(&id, &reason)?;
    Ok(Json(serde_json::to_value(s).unwrap_or_default()))
}

/// `POST /v1/executions/{id}/signals/{name}` with the payload as the body
/// (an empty body sends `{}`).
async fn send_signal(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path((id, name)): Path<(String, String)>,
    body: Option<Json<Value>>,
) -> ApiResult<Json<Value>> {
    allow_execution(&state, &caller, "signal", &id)?;
    let payload = body.map(|b| b.0).unwrap_or_else(|| json!({}));
    let seq = engine(&state).signal(&id, &name, payload)?;
    Ok(Json(json!({
        "execution_id": id,
        "signal": name,
        "accepted": true,
        "seq": seq,
    })))
}

/// `GET /v1/executions/{id}/queries/{name}`: evaluate a declared query.
async fn run_query(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path((id, name)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    allow_execution(&state, &caller, "query", &id)?;
    let value = engine(&state).query(&id, &name)?;
    Ok(Json(
        json!({ "execution_id": id, "query": name, "value": value }),
    ))
}

#[derive(Deserialize)]
struct WaitQuery {
    timeout_ms: Option<u64>,
}

/// Long-poll until the execution reaches a terminal status (or the timeout).
async fn wait_execution(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    Query(q): Query<WaitQuery>,
) -> ApiResult<Json<Value>> {
    allow_execution(&state, &caller, "read", &id)?;
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(q.timeout_ms.unwrap_or(30_000).min(300_000));
    loop {
        let notified = state.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let s = engine(&state).execution(&id)?;
        if s.status.is_terminal() || tokio::time::Instant::now() >= deadline {
            return Ok(Json(serde_json::to_value(s).unwrap_or_default()));
        }
        let _ = tokio::time::timeout_at(deadline, notified).await;
    }
}

async fn list_workers(State(state): State<Shared>) -> ApiResult<Json<Value>> {
    Ok(Json(json!({ "workers": engine(&state).workers()? })))
}

async fn register_worker(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Json(req): Json<RegisterWorker>,
) -> ApiResult<Json<Value>> {
    if let Some(id) = caller.identity() {
        if req.worker_id != id.id {
            return Err(forbidden(format!(
                "this credential belongs to worker `{}`, not `{}`",
                id.id, req.worker_id
            )));
        }
        if let Some(rt) = &id.runtime {
            if !rt.eq_ignore_ascii_case(&req.runtime) {
                return Err(forbidden(format!(
                    "worker `{}` must announce runtime `{rt}`",
                    id.id
                )));
            }
        }
        let denied: Vec<&str> = req
            .contracts
            .iter()
            .map(|c| c.task_name.as_str())
            .filter(|t| !id.may_serve(t))
            .collect();
        if !denied.is_empty() {
            return Err(forbidden(format!(
                "worker `{}` may not serve: {} (allowed: {})",
                id.id,
                denied.join(", "),
                id.tasks.join(", ")
            )));
        }
    }
    Ok(Json(
        serde_json::to_value(engine(&state).register_worker(req)?).unwrap_or_default(),
    ))
}

#[derive(Deserialize)]
struct SessionRequest {
    worker_session_id: String,
    #[serde(default)]
    wait_ms: Option<u64>,
}

async fn heartbeat(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Json(req): Json<SessionRequest>,
) -> ApiResult<Json<Value>> {
    authorize_session(&state, &caller, &req.worker_session_id)?;
    Ok(Json(
        serde_json::to_value(engine(&state).heartbeat(&req.worker_session_id)?).unwrap_or_default(),
    ))
}

async fn unregister(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Json(req): Json<SessionRequest>,
) -> ApiResult<Json<Value>> {
    authorize_session(&state, &caller, &req.worker_session_id)?;
    engine(&state).unregister_worker(&req.worker_session_id)?;
    Ok(Json(json!({ "accepted": true })))
}

/// Long-poll for a task. Returns 204 when nothing arrived within `wait_ms`.
async fn poll(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Json(req): Json<SessionRequest>,
) -> ApiResult<Response> {
    authorize_session(&state, &caller, &req.worker_session_id)?;
    let wait = Duration::from_millis(req.wait_ms.unwrap_or(20_000).min(60_000));
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let notified = state.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let task: Option<TaskAssignment> = engine(&state).poll(&req.worker_session_id)?;
        if let Some(t) = task {
            return Ok(Json(serde_json::to_value(t).unwrap_or_default()).into_response());
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        let _ = tokio::time::timeout_at(deadline, notified).await;
    }
}

#[derive(Deserialize)]
struct CompleteRequest {
    token: String,
    #[serde(default)]
    output: Value,
}

async fn complete_task(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Json(req): Json<CompleteRequest>,
) -> ApiResult<Json<Value>> {
    if req.token.is_empty() {
        return Err(bad_request("token is required"));
    }
    authorize_token(&state, &caller, &req.token)?;
    Ok(Json(
        serde_json::to_value(engine(&state).complete(&req.token, req.output)?).unwrap_or_default(),
    ))
}

#[derive(Deserialize)]
struct FailRequest {
    token: String,
    error_code: String,
    #[serde(default)]
    error_message: String,
    #[serde(default = "default_true")]
    retryable: bool,
    #[serde(default)]
    details: Option<Value>,
}

fn default_true() -> bool {
    true
}

async fn fail_task(
    State(state): State<Shared>,
    Extension(caller): Extension<Caller>,
    Json(req): Json<FailRequest>,
) -> ApiResult<Json<Value>> {
    if req.error_code.is_empty() {
        return Err(bad_request("error_code is required"));
    }
    authorize_token(&state, &caller, &req.token)?;
    let error = TaskError {
        code: req.error_code,
        message: req.error_message,
        retryable: req.retryable,
        details: req.details,
    };
    Ok(Json(
        serde_json::to_value(engine(&state).fail(&req.token, error)?).unwrap_or_default(),
    ))
}

/// Background loop: timers, retries, deadlines, liveness and built-in tasks.
pub async fn run_ticker(state: Shared, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let jobs = match engine(&state).tick() {
            Ok(j) => j,
            Err(e) => {
                tracing::error!(error = %e, "tick failed");
                continue;
            }
        };
        for job in jobs {
            let st = state.clone();
            tokio::spawn(async move {
                let input = job.input.clone();
                let timeout = job.timeout_ms;
                let result = tokio::task::spawn_blocking(move || {
                    crate::http_adapter::execute(&input, timeout)
                })
                .await
                .unwrap_or_else(|e| Err(TaskError::new("INTERNAL", e.to_string(), true)));
                let mut e = engine(&st);
                let r = match result {
                    Ok(out) => e.complete(&job.token, out),
                    Err(err) => e.fail(&job.token, err),
                };
                if let Err(err) = r {
                    tracing::error!(error = %err, "recording built-in task result failed");
                }
            });
        }
    }
}

/// PEM files for serving HTTPS directly.
pub struct TlsConfig {
    pub cert: std::path::PathBuf,
    pub key: std::path::PathBuf,
    /// Enables mutual TLS: client certificates signed by this CA are verified
    /// and identify workers. Clients without a certificate can still connect
    /// and authenticate with a token.
    pub client_ca: Option<std::path::PathBuf>,
}

fn io_err(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, msg)
}

fn read_pem_certs(
    path: &std::path::Path,
) -> std::io::Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let pem =
        std::fs::read(path).map_err(|e| io_err(format!("cannot read {}: {e}", path.display())))?;
    let certs = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io_err(format!("{}: {e}", path.display())))?;
    if certs.is_empty() {
        return Err(io_err(format!(
            "{} contains no certificates",
            path.display()
        )));
    }
    Ok(certs)
}

/// Build the rustls server configuration (with client verification for mTLS).
fn rustls_config(tls: &TlsConfig) -> std::io::Result<rustls::ServerConfig> {
    let chain = read_pem_certs(&tls.cert)?;
    let key_pem = std::fs::read(&tls.key)
        .map_err(|e| io_err(format!("cannot read {}: {e}", tls.key.display())))?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|e| io_err(format!("{}: {e}", tls.key.display())))?
        .ok_or_else(|| io_err(format!("{} contains no private key", tls.key.display())))?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| io_err(e.to_string()))?;
    let builder = match &tls.client_ca {
        Some(ca) => {
            let mut roots = rustls::RootCertStore::empty();
            for c in read_pem_certs(ca)? {
                roots
                    .add(c)
                    .map_err(|e| io_err(format!("{}: {e}", ca.display())))?;
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                provider,
            )
            .allow_unauthenticated()
            .build()
            .map_err(|e| io_err(format!("client CA: {e}")))?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };
    let mut config = builder
        .with_single_cert(chain, key)
        .map_err(|e| io_err(format!("TLS certificate/key: {e}")))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

pub struct ServerConfig {
    pub addr: SocketAddr,
    pub auth_token: Option<String>,
    pub worker_token: Option<String>,
    pub identities: Identities,
    pub tls: Option<TlsConfig>,
    /// Also serve the gRPC binding of the worker protocol on this address.
    pub grpc_addr: Option<SocketAddr>,
    /// Pre-bound listeners (socket activation, tests): used instead of
    /// binding `addr` / `grpc_addr`.
    pub listener: Option<std::net::TcpListener>,
    pub grpc_listener: Option<std::net::TcpListener>,
    pub body_limit: usize,
    pub tick_interval: Duration,
}

pub async fn serve(
    engine: Engine,
    config: ServerConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    // Several rustls crypto backends are compiled in (via different
    // dependencies); choose one before any TLS configuration is built.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let changed = engine.changed.clone();
    let state = Arc::new(AppState {
        engine: Mutex::new(engine),
        changed,
        auth_token: config.auth_token,
        worker_token: config.worker_token,
        identities: config.identities,
    });
    let app = router(state.clone(), config.body_limit);
    let ticker = tokio::spawn(run_ticker(state.clone(), config.tick_interval));
    // One shutdown signal fanned out to every listener.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        let _ = stop_tx.send(true);
    });
    let stopped = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|v| *v).await;
    };
    let bind = |addr: SocketAddr| {
        std::net::TcpListener::bind(addr)
            .map_err(|e| std::io::Error::new(e.kind(), format!("cannot listen on {addr}: {e}")))
    };
    let grpc_listener = match (config.grpc_listener, config.grpc_addr) {
        (Some(l), _) => Some(l),
        (None, Some(addr)) => Some(bind(addr)?),
        (None, None) => None,
    };
    let http_listener = match config.listener {
        Some(l) => l,
        None => bind(config.addr)?,
    };
    http_listener.set_nonblocking(true)?;
    let http_addr = http_listener.local_addr()?;
    let grpc = match grpc_listener {
        Some(std_listener) => {
            std_listener.set_nonblocking(true)?;
            let addr = std_listener.local_addr()?;
            let listener = tokio::net::TcpListener::from_std(std_listener)?;
            let incoming =
                tonic::transport::server::TcpIncoming::from_listener(listener, true, None)
                    .map_err(|e| io_err(format!("gRPC listener: {e}")))?;
            let mut builder = tonic::transport::Server::builder();
            if let Some(tls) = &config.tls {
                let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
                let read = |p: &std::path::Path| {
                    std::fs::read(p)
                        .map_err(|e| io_err(format!("cannot read {}: {e}", p.display())))
                };
                let mut t = tonic::transport::ServerTlsConfig::new().identity(
                    tonic::transport::Identity::from_pem(read(&tls.cert)?, read(&tls.key)?),
                );
                if let Some(ca) = &tls.client_ca {
                    t = t
                        .client_ca_root(tonic::transport::Certificate::from_pem(read(ca)?))
                        .client_auth_optional(true);
                }
                builder = builder
                    .tls_config(t)
                    .map_err(|e| io_err(format!("gRPC TLS: {e}")))?;
            }
            let service = crate::grpc::GrpcWorkers::new(state.clone());
            let rx = stop_rx.clone();
            tracing::info!(addr = %addr, tls = config.tls.is_some(), "gRPC worker protocol listening");
            Some(tokio::spawn(async move {
                if let Err(e) = builder
                    .add_service(service)
                    .serve_with_incoming_shutdown(incoming, stopped(rx))
                    .await
                {
                    tracing::error!(error = %e, "gRPC server failed");
                }
            }))
        }
        None => None,
    };
    let shutdown = stopped(stop_rx);
    let result = match config.tls {
        None => {
            let listener = tokio::net::TcpListener::from_std(http_listener)?;
            tracing::info!(addr = %http_addr, tls = false, "ORBIT runtime listening");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
        }
        Some(tls) => {
            // Several rustls crypto backends are compiled in (via different
            // dependencies); pick one explicitly so the choice is not ambiguous.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let rustls =
                axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(rustls_config(&tls)?));
            let handle = axum_server::Handle::new();
            let h = handle.clone();
            tokio::spawn(async move {
                shutdown.await;
                h.graceful_shutdown(Some(Duration::from_secs(10)));
            });
            tracing::info!(addr = %http_addr, tls = true, mtls = tls.client_ca.is_some(), "ORBIT runtime listening");
            axum_server::from_tcp_rustls(http_listener, rustls)?
                .handle(handle)
                .serve(app.into_make_service())
                .await
        }
    };
    ticker.abort();
    if let Some(g) = grpc {
        let _ = g.await;
    }
    result
}
