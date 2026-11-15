# Observability

The runtime exposes three kinds of signals: structured logs on stderr, Prometheus metrics at `/v1/metrics`, and OpenTelemetry traces exported over OTLP/HTTP. Each execution's full history is also available on demand through the CLI and the API. This page describes each signal, every metric, the span structure and attributes of traces, and how to configure them.

## Logs

The server logs to stderr. Choose the format with `--log-format`:

`text` (default):

```text
2026-11-15T09:38:33.954506Z  INFO orbit_runtime::engine: recovered active executions from history executions=1 in_flight=0
2026-11-15T09:38:33.954823Z  INFO orbit: starting ORBIT runtime db=/var/lib/orbit/orbit.db backend="sqlite" http_adapter=false auth=false
2026-11-15T09:38:33.955798Z  INFO orbit_runtime::server: ORBIT runtime listening addr=127.0.0.1:7233 tls=false
2026-11-15T09:38:34.957450Z  INFO orbit_runtime::engine: worker registered worker_id=py-1 runtime=python tasks=1
2026-11-15T09:38:34.990494Z  INFO orbit_runtime::engine: execution started execution_id=trace-1 workflow=Plain version=1.0.0
2026-11-15T09:38:35.075550Z  INFO orbit_runtime::engine: execution completed execution_id=trace-1
```

`json`, one object per line, for log pipelines:

```json
{"timestamp":"2026-11-15T09:38:18.766858Z","level":"INFO","fields":{"message":"starting ORBIT runtime","db":"orbit.db","backend":"sqlite","http_adapter":false,"auth":false},"target":"orbit"}
{"timestamp":"2026-11-15T09:38:18.768141Z","level":"INFO","fields":{"message":"ORBIT runtime listening","addr":"127.0.0.1:7233","tls":false},"target":"orbit_runtime::server"}
{"timestamp":"2026-11-15T09:38:19.759674Z","level":"INFO","fields":{"message":"shutting down"},"target":"orbit"}
```

### Log levels and filtering

`ORBIT_LOG` sets the filter, in `tracing-subscriber` `EnvFilter` syntax. The default is `info`. Setting `ORBIT_LOG` replaces the default, so include a base level:

```bash
ORBIT_LOG=info,orbit_runtime=debug orbit server   # every committed event at debug level
ORBIT_LOG=warn orbit server                       # warnings and errors only
```

At `debug`, every committed event is logged:

```text
DEBUG orbit_runtime::engine: event execution_id=dbg-1 seq=3 event="StepScheduled" step="work"
```

### Log messages

| Level | Message | Fields |
|---|---|---|
| INFO | `starting ORBIT runtime` | `db` (password redacted), `backend`, `http_adapter`, `auth` |
| INFO | `ORBIT runtime listening` | `addr`, `tls`, `mtls` (with TLS) |
| INFO | `gRPC worker protocol listening` | `addr`, `tls` |
| INFO | `exporting OpenTelemetry traces` | `endpoint`, `service` |
| INFO | `recovered active executions from history` | `executions`, `in_flight` |
| INFO | `registered workflow definition` | `workflow`, `version`, `hash` |
| INFO | `execution started` | `execution_id`, `workflow`, `version` |
| INFO | `execution completed`, `execution cancelled` | `execution_id` |
| WARN | `execution failed` | `execution_id`, `code`, `error`, `compensation` |
| WARN | `execution timed out` | `execution_id` |
| WARN | `step failed` | `execution_id`, `step`, `code`, `error` |
| INFO | `retry scheduled` | `execution_id`, `step`, `attempt`, `delay_ms` |
| WARN | `attempt outcome uncertain; redelivering` | `execution_id`, `step`, `attempt`, `reason` |
| INFO | `cancelling child workflow because its parent no longer needs it` | `execution_id` |
| INFO | `worker registered` | `worker_id`, `runtime`, `tasks` |
| INFO | `worker unregistered` | `worker_id` |
| WARN | `reconnected to PostgreSQL` | |
| ERROR | `PostgreSQL reconnect failed` | `error` |
| WARN | `OTLP collector rejected spans` | `status` |
| WARN | `cannot export spans to the OTLP collector` | `error` |
| WARN | `OTLP export queue is full; dropping spans` | |
| ERROR | `tick failed`, `recording built-in task result failed` | `error` |
| INFO | `shutting down` | |
| DEBUG | `event` | `execution_id`, `seq`, `event`, `step` |

Logs never include workflow inputs, step inputs, outputs or error details beyond the error code and message.

## Prometheus metrics

`GET /v1/metrics` returns Prometheus text format (`content-type: text/plain; version=0.0.4`). It requires the admin token when authentication is enabled.

```yaml
# prometheus.yml
scrape_configs:
  - job_name: orbit
    metrics_path: /v1/metrics
    scheme: https
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/orbit-token
    tls_config:
      ca_file: /etc/prometheus/orbit-ca.pem
    static_configs:
      - targets: ["orbit.internal:7233"]
```

### Metric reference

The metrics have no labels. Counters are derived from committed events, are kept in memory, and start from zero when the server starts; Prometheus handles the reset in `rate()` and `increase()`.

| Metric | Type | Incremented by | Meaning |
|---|---|---|---|
| `orbit_executions_started_total` | counter | `WorkflowCreated` | Executions started, including child workflows. |
| `orbit_executions_completed_total` | counter | `WorkflowCompleted` | Executions completed. |
| `orbit_executions_failed_total` | counter | `WorkflowFailed` | Executions failed. |
| `orbit_executions_cancelled_total` | counter | `WorkflowCancelled` | Executions cancelled. |
| `orbit_executions_timed_out_total` | counter | `WorkflowTimedOut` | Executions timed out. |
| `orbit_tasks_dispatched_total` | counter | `StepDispatched` | Task attempts dispatched, including redeliveries, child workflow starts and built-in tasks. |
| `orbit_tasks_succeeded_total` | counter | `StepSucceeded` | Units that succeeded, including compensations and completed `for` steps. |
| `orbit_task_attempts_failed_total` | counter | `StepAttemptFailed` | Failed attempts, whether or not they are retried. |
| `orbit_task_attempts_uncertain_total` | counter | `StepAttemptUncertain` | Attempts with an unknown outcome, redelivered. |
| `orbit_retries_scheduled_total` | counter | `RetryScheduled` | Retries scheduled. |
| `orbit_steps_skipped_total` | counter | `StepSkipped` | Steps skipped by `when` conditions. |
| `orbit_stale_results_total` | counter | `StaleResultIgnored` | Duplicate or late results ignored. |
| `orbit_compensations_started_total` | counter | `CompensationStarted` | Executions that started compensating. |
| `orbit_compensations_failed_total` | counter | `CompensationFailed` | Compensations that failed permanently (one per failed compensation). |
| `orbit_events_committed_total` | counter | every event | History events committed. |
| `orbit_active_executions` | gauge | | Executions not yet finished. |
| `orbit_worker_sessions` | gauge | | Worker sessions the runtime remembers. A silent session is forgotten after four session timeouts. |

Example output:

```text
# HELP orbit_executions_started_total Executions started
# TYPE orbit_executions_started_total counter
orbit_executions_started_total 8
# HELP orbit_executions_completed_total Executions completed
# TYPE orbit_executions_completed_total counter
orbit_executions_completed_total 2
# HELP orbit_executions_failed_total Executions failed
# TYPE orbit_executions_failed_total counter
orbit_executions_failed_total 4
...
# HELP orbit_active_executions Executions not yet finished
# TYPE orbit_active_executions gauge
orbit_active_executions 0
# HELP orbit_worker_sessions Registered worker sessions
# TYPE orbit_worker_sessions gauge
orbit_worker_sessions 2
```

### Suggested alerts

| Condition | Expression |
|---|---|
| Executions failing | `rate(orbit_executions_failed_total[5m]) > 0` |
| Compensation failing (needs a person) | `increase(orbit_compensations_failed_total[15m]) > 0` |
| Workers going silent | `rate(orbit_task_attempts_uncertain_total[5m]) > 0` |
| No workers connected | `orbit_worker_sessions == 0` |
| Runtime down | `up{job="orbit"} == 0` |

For backlog per task (tasks scheduled with no worker to take them), use `queue_timeout` on steps and alert on failures with code `QUEUE_TIMEOUT` from your logs, since the metrics have no per-task labels.

## OpenTelemetry tracing

The runtime can export one trace per execution over OTLP/HTTP with JSON encoding, which the OpenTelemetry Collector, Jaeger, Grafana Tempo, Honeycomb and other OTLP backends accept.

### Configuration

| Setting | Meaning |
|---|---|
| `--otlp-endpoint URL` or `OTEL_EXPORTER_OTLP_ENDPOINT` | Base URL of the collector, for example `http://localhost:4318`. Spans are posted to `<URL>/v1/traces`. Export is off when unset. |
| `OTEL_EXPORTER_OTLP_HEADERS` | Extra request headers as `key=value` pairs separated by commas, for example an API key. |
| `OTEL_SERVICE_NAME` | `service.name` resource attribute. Default `orbit-runtime`. |

```bash
OTEL_SERVICE_NAME=orbit-prod \
OTEL_EXPORTER_OTLP_HEADERS="x-api-key=$TRACING_KEY" \
orbit server --token "$ORBIT_TOKEN" --otlp-endpoint http://otel-collector:4318
```

Every request carries the resource attributes `service.name` and `service.version` (the runtime version), and the instrumentation scope `orbit-runtime`.

### Span structure

```text
trace  (one per top-level execution)
└── workflow OrderProcessing                     root span, execution start to end
    ├── step validate_order                      one span per attempt
    ├── step analyze_risk        (error)         attempt 1 failed
    ├── step analyze_risk                        attempt 2
    ├── step reserve_inventory
    ├── step process_payment     (error)
    ├── step compensate:reserve_inventory        compensation
    └── step ship                                child workflow step
        └── workflow Shipping                    child execution, same trace
            ├── step quote
            └── step book
```

- **Root span** `workflow <Name>`: from `WorkflowCreated` to the terminal event. Exported when the execution finishes.
- **Unit spans** `step <unit key>`: one per attempt of a task, from dispatch to result; one per `sleep` timer, from start to fire; one per compensation attempt (`step compensate:<unit>`). Exported when the attempt ends: succeeded, failed or uncertain. Parent: the root span.
- **Child workflows** join their parent's trace. The child's root span is a child of the parent step's span.
- Attempts still running when an execution ends are not exported, because they have no end time.

All spans have kind `INTERNAL` (`1`). A span's status is `OK` (`1`) or `ERROR` (`2`) with a message:

| Span | Error when | Status message |
|---|---|---|
| Unit | the attempt failed | `<code>: <message>`, for example `CARD_DECLINED: card was declined` |
| Unit | the outcome is uncertain | `outcome unknown: <reason>` |
| Root | the execution did not complete | `<code>: <message>` of the execution error, or the status (`CANCELLED`, `TIMED_OUT`) |

### Attributes

| Attribute | Type | On | Meaning |
|---|---|---|---|
| `orbit.execution_id` | string | all | Execution id. |
| `orbit.workflow` | string | all | Workflow name. |
| `orbit.workflow_version` | string | root | Workflow version. |
| `orbit.status` | string | root | Final status, for example `COMPLETED`. |
| `orbit.compensation` | string | root | `none`, `completed` or `failed`, when the execution was aborted. |
| `orbit.unit` | string | unit | Unit key, for example `reserve[2]` or `compensate:reserve_inventory`. |
| `orbit.attempt` | integer | unit | Attempt number. |
| `orbit.outcome` | string | unit | `succeeded`, `failed`, `uncertain` or `timer fired`. |
| `orbit.worker_id` | string | unit | Worker that ran the attempt, when it was dispatched. |
| `orbit.redelivery` | boolean | unit | Present and `true` when the attempt was a redelivery. |

### Identifiers

Trace and span ids are derived from execution and unit identifiers with SHA-256, so a runtime restart never produces duplicate or orphaned spans:

| Id | Derived from |
|---|---|
| Trace id (16 bytes) | `orbit-trace:<top-level execution id>` |
| Root span id (8 bytes) | `orbit-span:<execution id>` |
| Unit span id (8 bytes) | `orbit-span:<execution id>/<unit key>#<dispatch number>` |

Because the trace id is a function of the execution id, you can compute it to find an execution's trace. In Python:

```python
import hashlib
trace_id = hashlib.sha256(b"orbit-trace:order-1002").hexdigest()[:32]
```

### Example span

A unit span as received by a collector:

```json
{
  "traceId": "6d61feb6ff6a03af93dec3e7836b3492",
  "spanId": "5e8f395157c28e15",
  "parentSpanId": "4c82e46ef6bc6222",
  "name": "step work",
  "kind": 1,
  "startTimeUnixNano": "1794735515047000000",
  "endTimeUnixNano": "1794735515072000000",
  "attributes": [
    {"key": "orbit.execution_id", "value": {"stringValue": "trace-1"}},
    {"key": "orbit.workflow", "value": {"stringValue": "Plain"}},
    {"key": "orbit.unit", "value": {"stringValue": "work"}},
    {"key": "orbit.attempt", "value": {"intValue": "1"}},
    {"key": "orbit.outcome", "value": {"stringValue": "succeeded"}},
    {"key": "orbit.worker_id", "value": {"stringValue": "py-1"}}
  ],
  "status": {"code": 1}
}
```

### Export behavior

- Spans are exported only after the events they describe are committed.
- Export runs on a background thread and never blocks the engine. Spans queue in memory (up to 1024 batches) and are sent in requests of up to about 512 spans, with a 10-second timeout.
- If the collector is unreachable or rejects a request, the spans of that request are dropped and a warning is logged. If the queue is full, new spans are dropped. Tracing is best-effort; the history is the complete record.
- Timestamps have millisecond precision.
- Worker SDKs do not create spans of their own in this version. Correlate worker logs with traces through `execution_id`, `step_id` and `attempt`, which every task carries.

## Execution history

For any single execution, the history is the most detailed record: every event, in order, with its inputs and outputs. Use `orbit status`, `orbit logs`, `orbit inspect` and `orbit ps`, or the [HTTP API](../reference/http-api.md). See [Executions and history](executions.md#inspecting-executions).

## See also

- [Executions and history](executions.md)
- [Running the server](server.md)
- [HTTP API: metrics](../reference/http-api.md#get-v1metrics)
- [Environment variables](../reference/environment.md)
- [Deployment](deployment.md)
