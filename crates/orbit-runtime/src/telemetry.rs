//! OpenTelemetry tracing: OTLP/HTTP (JSON encoding) export of execution
//! traces, accepted by the OpenTelemetry Collector, Jaeger, Grafana Tempo,
//! Honeycomb and other OTLP backends.
//!
//! * One trace per workflow execution; child workflows join their parent's
//!   trace, nested under the step that started them.
//! * A root span per execution, and one span per task attempt (dispatch to
//!   result), timer and compensation, with worker, attempt and error attributes.
//! * Trace and span ids are derived deterministically (SHA-256) from
//!   execution, unit and dispatch identifiers, so a runtime restart never
//!   produces duplicate or orphaned spans.
//! * Export never blocks the engine: spans are queued to a background thread
//!   and dropped (with a warning) if the collector cannot keep up.
//!
//! Configured with `orbit server --otlp-endpoint`, or the standard
//! `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS` and
//! `OTEL_SERVICE_NAME` environment variables.

use crate::model::{Event, ExecutionState, HistoryEvent, WorkflowStatus};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// Base OTLP/HTTP endpoint, e.g. `http://localhost:4318`; spans are posted
    /// to `<endpoint>/v1/traces`.
    pub endpoint: String,
    /// Extra request headers, e.g. an API key.
    pub headers: Vec<(String, String)>,
    pub service_name: String,
}

impl TelemetryConfig {
    /// From `OTEL_EXPORTER_OTLP_ENDPOINT` / `_HEADERS` and `OTEL_SERVICE_NAME`.
    pub fn from_env() -> Option<TelemetryConfig> {
        let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
            .ok()
            .filter(|e| !e.is_empty())?;
        Some(TelemetryConfig::new(&endpoint))
    }

    pub fn new(endpoint: &str) -> TelemetryConfig {
        let headers = std::env::var("OTEL_EXPORTER_OTLP_HEADERS")
            .unwrap_or_default()
            .split(',')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        TelemetryConfig {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            headers,
            service_name: std::env::var("OTEL_SERVICE_NAME")
                .unwrap_or_else(|_| "orbit-runtime".into()),
        }
    }
}

/// Where a trace and its root span hang: the trace id and the parent span id
/// (the parent workflow's step span, for child workflows).
#[derive(Debug, Clone)]
pub struct TraceContext {
    pub trace_id: String,
    pub parent_span_id: Option<String>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

/// Trace id of a top-level execution.
pub fn trace_id(root_execution: &str) -> String {
    hex(&digest(&format!("orbit-trace:{root_execution}"))[..16])
}

pub fn root_span_id(execution: &str) -> String {
    hex(&digest(&format!("orbit-span:{execution}"))[..8])
}

/// Span id of the `dispatch`-th dispatch of `unit` in `execution`.
pub fn unit_span_id(execution: &str, unit: &str, dispatch: u32) -> String {
    hex(&digest(&format!("orbit-span:{execution}/{unit}#{dispatch}"))[..8])
}

fn attr(key: &str, value: impl Into<Value>) -> Value {
    let v: Value = value.into();
    let typed = match &v {
        Value::Bool(b) => json!({ "boolValue": b }),
        Value::Number(n) if n.is_i64() || n.is_u64() => json!({ "intValue": n.to_string() }),
        Value::Number(n) => json!({ "doubleValue": n }),
        Value::String(s) => json!({ "stringValue": s }),
        other => json!({ "stringValue": other.to_string() }),
    };
    json!({ "key": key, "value": typed })
}

fn nanos(ms: u64) -> String {
    (ms as u128 * 1_000_000).to_string()
}

/// One finished span, before OTLP encoding.
struct Span<'a> {
    trace: &'a str,
    id: &'a str,
    parent: Option<&'a str>,
    name: String,
    start_ms: u64,
    end_ms: u64,
    attrs: Vec<Value>,
    error: Option<String>,
}

impl Span<'_> {
    fn encode(self) -> Value {
        let mut s = json!({
            "traceId": self.trace,
            "spanId": self.id,
            "name": self.name,
            "kind": 1,
            "startTimeUnixNano": nanos(self.start_ms),
            "endTimeUnixNano": nanos(self.end_ms.max(self.start_ms)),
            "attributes": self.attrs,
            "status": match &self.error {
                Some(m) => json!({ "code": 2, "message": m }),
                None => json!({ "code": 1 }),
            },
        });
        if let Some(p) = self.parent {
            s["parentSpanId"] = json!(p);
        }
        s
    }
}

/// Spans finished by one committed batch of events.
pub fn spans_for(state: &ExecutionState, ctx: &TraceContext, batch: &[HistoryEvent]) -> Vec<Value> {
    let root = root_span_id(&state.id);
    let mut out = vec![];
    for h in batch {
        let ended = |key: &str, error: Option<String>, outcome: &str| -> Option<Value> {
            let u = state.units.get(key)?;
            let start = u.dispatched_at_ms.or(u.scheduled_at_ms)?;
            let mut attrs = vec![
                attr("orbit.execution_id", state.id.clone()),
                attr("orbit.workflow", state.workflow.clone()),
                attr("orbit.unit", key.to_string()),
                attr("orbit.attempt", u.attempt),
                attr("orbit.outcome", outcome.to_string()),
            ];
            if let Some(w) = &u.worker_id {
                attrs.push(attr("orbit.worker_id", w.clone()));
            }
            if u.redelivery {
                attrs.push(attr("orbit.redelivery", true));
            }
            // Later runs (continue-as-new) reuse unit keys: keep their spans apart.
            let id = if state.run > 1 {
                unit_span_id(
                    &state.id,
                    &format!("run{}/{key}", state.run),
                    u.dispatch_count.max(1),
                )
            } else {
                unit_span_id(&state.id, key, u.dispatch_count.max(1))
            };
            Some(
                Span {
                    trace: &ctx.trace_id,
                    id: &id,
                    parent: Some(&root),
                    name: format!("step {key}"),
                    start_ms: start,
                    end_ms: h.timestamp_ms,
                    attrs,
                    error,
                }
                .encode(),
            )
        };
        let s = match &h.event {
            Event::StepSucceeded { step, .. } => ended(step, None, "succeeded"),
            Event::TimerFired { step } => ended(step, None, "timer fired"),
            Event::StepAttemptFailed { step, error, .. } => ended(
                step,
                Some(format!("{}: {}", error.code, error.message)),
                "failed",
            ),
            Event::StepAttemptUncertain { step, reason, .. } => ended(
                step,
                Some(format!("outcome unknown: {reason}")),
                "uncertain",
            ),
            Event::WorkflowCompleted { .. }
            | Event::WorkflowFailed { .. }
            | Event::WorkflowCancelled { .. }
            | Event::WorkflowTimedOut { .. } => {
                let mut attrs = vec![
                    attr("orbit.execution_id", state.id.clone()),
                    attr("orbit.workflow", state.workflow.clone()),
                    attr("orbit.workflow_version", state.version.clone()),
                    attr("orbit.status", state.status.as_str().to_string()),
                ];
                if let Some(c) = state.compensation {
                    attrs.push(attr("orbit.compensation", format!("{c:?}").to_lowercase()));
                }
                let error = match state.status {
                    WorkflowStatus::Completed => None,
                    _ => Some(
                        state
                            .error
                            .as_ref()
                            .map(|e| format!("{}: {}", e.code, e.message))
                            .unwrap_or_else(|| state.status.as_str().to_string()),
                    ),
                };
                Some(
                    Span {
                        trace: &ctx.trace_id,
                        id: &root,
                        parent: ctx.parent_span_id.as_deref(),
                        name: format!("workflow {}", state.workflow),
                        start_ms: state.created_at_ms,
                        end_ms: h.timestamp_ms,
                        attrs,
                        error,
                    }
                    .encode(),
                )
            }
            _ => None,
        };
        out.extend(s);
    }
    // Attempts still running when the workflow ends are not exported: they did
    // not finish, so there is no honest end time for them.
    out
}

/// Background OTLP exporter.
pub struct Exporter {
    tx: mpsc::SyncSender<Vec<Value>>,
}

impl Exporter {
    pub fn start(config: TelemetryConfig) -> Exporter {
        let (tx, rx) = mpsc::sync_channel::<Vec<Value>>(1024);
        std::thread::Builder::new()
            .name("orbit-otlp".into())
            .spawn(move || {
                let agent: ureq::Agent = ureq::Agent::config_builder()
                    .http_status_as_error(false)
                    .timeout_global(Some(Duration::from_secs(10)))
                    .build()
                    .into();
                let url = format!("{}/v1/traces", config.endpoint);
                while let Ok(first) = rx.recv() {
                    // Batch whatever else is already queued.
                    let mut spans = first;
                    while let Ok(more) = rx.try_recv() {
                        spans.extend(more);
                        if spans.len() >= 512 {
                            break;
                        }
                    }
                    let body = json!({
                        "resourceSpans": [{
                            "resource": { "attributes": [
                                attr("service.name", config.service_name.clone()),
                                attr("service.version", env!("CARGO_PKG_VERSION")),
                            ]},
                            "scopeSpans": [{ "scope": { "name": "orbit-runtime", "version": env!("CARGO_PKG_VERSION") }, "spans": spans }],
                        }]
                    });
                    let mut req = agent.post(&url).header("content-type", "application/json");
                    for (k, v) in &config.headers {
                        req = req.header(k, v);
                    }
                    match req.send(body.to_string()) {
                        Ok(r) if r.status().is_success() => {}
                        Ok(r) => tracing::warn!(status = r.status().as_u16(), "OTLP collector rejected spans"),
                        Err(e) => tracing::warn!(error = %e, "cannot export spans to the OTLP collector"),
                    }
                }
            })
            .expect("start OTLP exporter thread");
        Exporter { tx }
    }

    pub fn export(&self, spans: Vec<Value>) {
        if spans.is_empty() {
            return;
        }
        if self.tx.try_send(spans).is_err() {
            tracing::warn!("OTLP export queue is full; dropping spans");
        }
    }
}
