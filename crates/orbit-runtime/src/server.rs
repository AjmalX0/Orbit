//! HTTP API (RFC-0001 §11.2) and the HTTP/JSON binding of the worker
//! protocol (§8). See `protocol/README.md` for the wire format.

use crate::engine::{Engine, EngineError, RegisterWorker, TaskAssignment};
use crate::model::TaskError;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
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
}

pub type Shared = Arc<AppState>;

pub struct ApiError(EngineError);

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

fn engine(state: &Shared) -> std::sync::MutexGuard<'_, Engine> {
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

/// Authentication. With an admin token configured, every endpoint except
/// `/v1/health` requires a bearer token. The admin token can call anything;
/// the optional worker token can only call the worker protocol endpoints, so
/// a compromised worker host cannot start, cancel or read executions.
async fn auth(State(state): State<Shared>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if let Some(admin) = &state.auth_token {
        if path != "/v1/health" {
            let presented = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::as_bytes);
            let is_admin = presented.is_some_and(|t| constant_time_eq(t, admin.as_bytes()));
            let is_worker = presented.is_some_and(|t| {
                state
                    .worker_token
                    .as_ref()
                    .is_some_and(|w| constant_time_eq(t, w.as_bytes()))
            });
            if !is_admin && !is_worker {
                return ApiError(EngineError::new(
                    401,
                    "UNAUTHENTICATED",
                    "missing or invalid bearer token",
                ))
                .into_response();
            }
            if !is_admin && !is_worker_path(path) {
                return ApiError(EngineError::new(
                    403,
                    "FORBIDDEN",
                    "the worker credential may only call /v1/workers/* and /v1/tasks/*",
                ))
                .into_response();
            }
        }
    }
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .insert("x-orbit-api-version", HeaderValue::from_static(API_VERSION));
    resp
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
        "ir_versions": [orbit_ir::IR_VERSION],
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

async fn list_workflows(State(state): State<Shared>) -> Json<Value> {
    let defs = engine(&state).definitions();
    Json(json!({ "workflows": defs.iter().map(|d| summary(d)).collect::<Vec<_>>() }))
}

async fn get_workflow(
    State(state): State<Shared>,
    Path((name, version)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
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
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
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
    Json(req): Json<StartRequest>,
) -> ApiResult<Response> {
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
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let list = engine(&state).list_executions(
        q.status.as_deref().map(str::to_uppercase).as_deref(),
        q.workflow.as_deref(),
        q.limit.unwrap_or(50),
    )?;
    Ok(Json(json!({ "executions": list })))
}

async fn get_execution(
    State(state): State<Shared>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let s = engine(&state).execution(&id)?;
    Ok(Json(serde_json::to_value(s).unwrap_or_default()))
}

async fn get_history(
    State(state): State<Shared>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
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
    Path(id): Path<String>,
    body: Option<Json<CancelRequest>>,
) -> ApiResult<Json<Value>> {
    let reason = body
        .and_then(|b| b.0.reason)
        .unwrap_or_else(|| "requested via API".into());
    let s = engine(&state).cancel(&id, &reason)?;
    Ok(Json(serde_json::to_value(s).unwrap_or_default()))
}

#[derive(Deserialize)]
struct WaitQuery {
    timeout_ms: Option<u64>,
}

/// Long-poll until the execution reaches a terminal status (or the timeout).
async fn wait_execution(
    State(state): State<Shared>,
    Path(id): Path<String>,
    Query(q): Query<WaitQuery>,
) -> ApiResult<Json<Value>> {
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
    Json(req): Json<RegisterWorker>,
) -> ApiResult<Json<Value>> {
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
    Json(req): Json<SessionRequest>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        serde_json::to_value(engine(&state).heartbeat(&req.worker_session_id)?).unwrap_or_default(),
    ))
}

async fn unregister(
    State(state): State<Shared>,
    Json(req): Json<SessionRequest>,
) -> ApiResult<Json<Value>> {
    engine(&state).unregister_worker(&req.worker_session_id)?;
    Ok(Json(json!({ "accepted": true })))
}

/// Long-poll for a task. Returns 204 when nothing arrived within `wait_ms`.
async fn poll(State(state): State<Shared>, Json(req): Json<SessionRequest>) -> ApiResult<Response> {
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
    Json(req): Json<CompleteRequest>,
) -> ApiResult<Json<Value>> {
    if req.token.is_empty() {
        return Err(bad_request("token is required"));
    }
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
    Json(req): Json<FailRequest>,
) -> ApiResult<Json<Value>> {
    if req.error_code.is_empty() {
        return Err(bad_request("error_code is required"));
    }
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
}

pub struct ServerConfig {
    pub addr: SocketAddr,
    pub auth_token: Option<String>,
    pub worker_token: Option<String>,
    pub tls: Option<TlsConfig>,
    pub body_limit: usize,
    pub tick_interval: Duration,
}

pub async fn serve(
    engine: Engine,
    config: ServerConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let changed = engine.changed.clone();
    let state = Arc::new(AppState {
        engine: Mutex::new(engine),
        changed,
        auth_token: config.auth_token,
        worker_token: config.worker_token,
    });
    let app = router(state.clone(), config.body_limit);
    let ticker = tokio::spawn(run_ticker(state, config.tick_interval));
    let result = match config.tls {
        None => {
            let listener = tokio::net::TcpListener::bind(config.addr).await?;
            tracing::info!(addr = %listener.local_addr()?, tls = false, "ORBIT runtime listening");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
        }
        Some(tls) => {
            // Several rustls crypto backends are compiled in (via different
            // dependencies); pick one explicitly so the choice is not ambiguous.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
            let rustls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key)
                .await
                .map_err(|e| {
                    std::io::Error::new(e.kind(), format!("loading TLS certificate/key: {e}"))
                })?;
            let handle = axum_server::Handle::new();
            let h = handle.clone();
            tokio::spawn(async move {
                shutdown.await;
                h.graceful_shutdown(Some(Duration::from_secs(10)));
            });
            tracing::info!(addr = %config.addr, tls = true, "ORBIT runtime listening");
            axum_server::bind_rustls(config.addr, rustls)
                .handle(handle)
                .serve(app.into_make_service())
                .await
        }
    };
    ticker.abort();
    result
}
