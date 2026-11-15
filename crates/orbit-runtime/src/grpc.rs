//! gRPC binding of the worker protocol (`orbit.worker.v1.WorkerService`).
//!
//! It shares the engine, the credentials, per-worker identities and the
//! authorization rules with the HTTP API, so the two bindings behave the
//! same. Errors carry the ORBIT error code in the `orbit-error-code`
//! metadata entry and as a `CODE: message` prefix.

use crate::engine::{ContractAnnouncement, EngineError, RegisterWorker};
use crate::model::TaskError;
use crate::server::{
    authorize_session, authorize_token, engine, identify, ApiError, Caller, Shared,
};
use serde_json::{Map, Number, Value as Json};
use std::time::Duration;
use tonic::{Request, Response, Status};

/// Generated protocol types and the client/server stubs.
pub mod pb {
    tonic::include_proto!("orbit.worker.v1");
}

use pb::worker_service_server::{WorkerService, WorkerServiceServer};

// ------------------------------------------------------- value conversion

/// `google.protobuf.Value` -> JSON. Whole numbers within ±2^53 become
/// integers, since protobuf carries every number as a double.
pub fn to_json(v: prost_types::Value) -> Json {
    use prost_types::value::Kind;
    match v.kind {
        None | Some(Kind::NullValue(_)) => Json::Null,
        Some(Kind::BoolValue(b)) => Json::Bool(b),
        Some(Kind::NumberValue(n)) => {
            if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_992.0 {
                Json::from(n as i64)
            } else {
                Number::from_f64(n).map(Json::Number).unwrap_or(Json::Null)
            }
        }
        Some(Kind::StringValue(s)) => Json::String(s),
        Some(Kind::ListValue(l)) => Json::Array(l.values.into_iter().map(to_json).collect()),
        Some(Kind::StructValue(s)) => Json::Object(
            s.fields
                .into_iter()
                .map(|(k, v)| (k, to_json(v)))
                .collect::<Map<_, _>>(),
        ),
    }
}

/// JSON -> `google.protobuf.Value`.
pub fn from_json(v: Json) -> prost_types::Value {
    use prost_types::value::Kind;
    let kind = match v {
        Json::Null => Kind::NullValue(0),
        Json::Bool(b) => Kind::BoolValue(b),
        Json::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
        Json::String(s) => Kind::StringValue(s),
        Json::Array(a) => Kind::ListValue(prost_types::ListValue {
            values: a.into_iter().map(from_json).collect(),
        }),
        Json::Object(o) => Kind::StructValue(prost_types::Struct {
            fields: o.into_iter().map(|(k, v)| (k, from_json(v))).collect(),
        }),
    };
    prost_types::Value { kind: Some(kind) }
}

fn status(e: EngineError) -> Status {
    let code = match e.status {
        400 => tonic::Code::InvalidArgument,
        401 => tonic::Code::Unauthenticated,
        403 => tonic::Code::PermissionDenied,
        404 => tonic::Code::NotFound,
        409 => tonic::Code::FailedPrecondition,
        413 | 429 => tonic::Code::ResourceExhausted,
        _ => tonic::Code::Internal,
    };
    let mut s = Status::new(code, format!("{}: {}", e.code, e.message));
    if let Ok(v) = e.code.parse() {
        s.metadata_mut().insert("orbit-error-code", v);
    }
    s
}

fn api(e: ApiError) -> Status {
    status(e.0)
}

// ------------------------------------------------------------- service

pub struct GrpcWorkers {
    state: Shared,
}

impl GrpcWorkers {
    pub fn new(state: Shared) -> WorkerServiceServer<GrpcWorkers> {
        WorkerServiceServer::new(GrpcWorkers { state })
    }

    /// Authenticate a call from its metadata and client certificate.
    fn caller<T>(&self, req: &Request<T>) -> Result<Caller, Status> {
        let bearer = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let certs = req.peer_certs();
        let der = certs
            .as_ref()
            .and_then(|c| c.first())
            .map(|c| c.as_ref().to_vec());
        match identify(&self.state, bearer, der.as_deref()).map_err(api)? {
            // The gRPC binding is the worker protocol: applications use HTTP.
            Some(Caller::Client(c)) => Err(status(EngineError::new(
                403,
                "FORBIDDEN",
                format!("client `{}` may not use the worker protocol", c.id),
            ))),
            Some(c) => Ok(c),
            None => Err(Status::unauthenticated(
                "UNAUTHENTICATED: missing or invalid credentials",
            )),
        }
    }

    fn identity_check_register(caller: &Caller, req: &RegisterWorker) -> Result<(), Status> {
        if let Caller::Worker(id) = caller {
            let deny = |m: String| Err(status(EngineError::new(403, "FORBIDDEN", m)));
            if req.worker_id != id.id {
                return deny(format!(
                    "this credential belongs to worker `{}`, not `{}`",
                    id.id, req.worker_id
                ));
            }
            if let Some(rt) = &id.runtime {
                if !rt.eq_ignore_ascii_case(&req.runtime) {
                    return deny(format!("worker `{}` must announce runtime `{rt}`", id.id));
                }
            }
            let denied: Vec<&str> = req
                .contracts
                .iter()
                .map(|c| c.task_name.as_str())
                .filter(|t| !id.may_serve(t))
                .collect();
            if !denied.is_empty() {
                return deny(format!(
                    "worker `{}` may not serve: {} (allowed: {})",
                    id.id,
                    denied.join(", "),
                    id.tasks.join(", ")
                ));
            }
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl WorkerService for GrpcWorkers {
    async fn register_worker(
        &self,
        req: Request<pb::RegisterWorkerRequest>,
    ) -> Result<Response<pb::RegisterWorkerResponse>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        let reg = RegisterWorker {
            worker_id: r.worker_id,
            runtime: r.runtime,
            sdk: (!r.sdk.is_empty()).then_some(r.sdk),
            contracts: r
                .contracts
                .into_iter()
                .map(|c| ContractAnnouncement {
                    task_name: c.task_name,
                    contract_version: c.contract_version,
                })
                .collect(),
        };
        Self::identity_check_register(&caller, &reg)?;
        let out = engine(&self.state).register_worker(reg).map_err(status)?;
        Ok(Response::new(pb::RegisterWorkerResponse {
            accepted: out.accepted,
            worker_session_id: out.worker_session_id,
            heartbeat_interval_ms: out.heartbeat_interval_ms,
            session_timeout_ms: out.session_timeout_ms,
            warnings: out.warnings,
        }))
    }

    async fn heartbeat(
        &self,
        req: Request<pb::HeartbeatRequest>,
    ) -> Result<Response<pb::HeartbeatResponse>, Status> {
        let caller = self.caller(&req)?;
        let session = req.into_inner().worker_session_id;
        authorize_session(&self.state, &caller, &session).map_err(api)?;
        let ack = engine(&self.state).heartbeat(&session).map_err(status)?;
        Ok(Response::new(pb::HeartbeatResponse {
            accepted: ack.accepted,
            cancel: ack.cancel,
        }))
    }

    async fn poll_task(
        &self,
        req: Request<pb::PollTaskRequest>,
    ) -> Result<Response<pb::ExecuteTaskRequest>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        authorize_session(&self.state, &caller, &r.worker_session_id).map_err(api)?;
        let wait = Duration::from_millis(if r.wait_ms == 0 {
            20_000
        } else {
            r.wait_ms.min(60_000)
        });
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.state.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(t) =
                crate::shared::poll(&self.state, &r.worker_session_id).map_err(status)?
            {
                return Ok(Response::new(pb::ExecuteTaskRequest {
                    token: t.token,
                    execution_id: t.execution_id,
                    workflow: t.workflow,
                    workflow_version: t.workflow_version,
                    step_id: t.step_id,
                    task_name: t.task_name,
                    contract_version: t.contract_version,
                    input: Some(from_json(t.input)),
                    idempotency_key: t.idempotency_key,
                    attempt: t.attempt,
                    redelivery: t.redelivery,
                    deadline_ms: t.deadline_ms,
                    timeout_ms: t.timeout_ms,
                }));
            }
            if tokio::time::Instant::now() >= deadline {
                // No task: an empty token (HTTP answers 204).
                return Ok(Response::new(pb::ExecuteTaskRequest::default()));
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }

    async fn complete_task(
        &self,
        req: Request<pb::CompleteTaskRequest>,
    ) -> Result<Response<pb::TaskResultAck>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        if r.token.is_empty() {
            return Err(Status::invalid_argument(
                "INVALID_REQUEST: token is required",
            ));
        }
        authorize_token(&self.state, &caller, &r.token).map_err(api)?;
        let output = r.output.map(to_json).unwrap_or(Json::Null);
        let ack = engine(&self.state)
            .complete(&r.token, output)
            .map_err(status)?;
        Ok(Response::new(pb::TaskResultAck {
            accepted: ack.accepted,
            reason: ack.reason.unwrap_or_default(),
        }))
    }

    async fn fail_task(
        &self,
        req: Request<pb::FailTaskRequest>,
    ) -> Result<Response<pb::TaskResultAck>, Status> {
        let caller = self.caller(&req)?;
        let r = req.into_inner();
        if r.error_code.is_empty() {
            return Err(Status::invalid_argument(
                "INVALID_REQUEST: error_code is required",
            ));
        }
        authorize_token(&self.state, &caller, &r.token).map_err(api)?;
        let error = TaskError {
            code: r.error_code,
            message: r.error_message,
            retryable: r.retryable,
            details: r.details.map(to_json).filter(|d| !d.is_null()),
        };
        let ack = engine(&self.state).fail(&r.token, error).map_err(status)?;
        Ok(Response::new(pb::TaskResultAck {
            accepted: ack.accepted,
            reason: ack.reason.unwrap_or_default(),
        }))
    }

    async fn unregister_worker(
        &self,
        req: Request<pb::UnregisterWorkerRequest>,
    ) -> Result<Response<pb::UnregisterWorkerResponse>, Status> {
        let caller = self.caller(&req)?;
        let session = req.into_inner().worker_session_id;
        authorize_session(&self.state, &caller, &session).map_err(api)?;
        engine(&self.state)
            .unregister_worker(&session)
            .map_err(status)?;
        Ok(Response::new(pb::UnregisterWorkerResponse {
            accepted: true,
        }))
    }
}
