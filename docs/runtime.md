# Runtime semantics (v0.1)

The ORBIT runtime (`orbit server`) is one process that owns all workflow state.
Workers are separate processes in any language. They never own workflow state.
They pull tasks, run business logic, and report results.

```text
              orbit CLI / apps ──HTTP(S)──┐
                                          ▼
 ┌────────────────────────── orbit server ───────────────────────────┐
 │  HTTP API ─┐                                                       │
 │  gRPC  ────┼─► Engine ─► decide(state, IR, now)  (pure)            │
 │            │     │  ▲                                              │
 │            │     ▼  │ fold(events)          OTLP traces ─► collector│
 │            │   SQLite or PostgreSQL: execution_events (authoritative)│
 └────────────┴──────▲─────────────────────────────────▲─────────────┘
         long-poll / results (HTTP/JSON or gRPC, optional mTLS)
      Python · TypeScript · Go · Rust · Java · C# workers
```

## Event sourcing

Every state change is an **event** appended to `execution_events`. The current
state of an execution is the left fold of its events (`ExecutionState::replay`).
The `workflow_executions` table is only a projection for listing. It is updated
in the same transaction as the events.

### Storage backends

| `--db` | Backend | Notes |
|---|---|---|
| `orbit.db` (any path; the default) | SQLite | single file, WAL mode, `synchronous=FULL` |
| `postgres://user:pass@host:5432/db` | PostgreSQL | schema created on first start; one connection on a dedicated thread, reconnected automatically |

Both backends store the same schema with the same guarantees. For PostgreSQL,
TLS follows the URL's `sslmode`: `prefer` (the default) uses TLS when the
server offers it, `require` insists on it, and `disable` turns it off.
Certificates are verified against the public web PKI, plus any CA bundle in
`ORBIT_DB_CA_CERT`. Unlike libpq's `prefer`, ORBIT never accepts an
unverifiable certificate: use `sslmode=disable` for a local server with a
self-signed certificate, or provide its CA. The URL's password is redacted in
logs.

The engine never mutates memory before the transaction commits. If the write
fails, the in-memory state is unchanged.

Decisions come from a **pure decider**, `decide(state, definition, now)`. It
returns the events that should happen next. The engine runs it to a fixpoint
and commits the result as one batch. Because the decider does no I/O and
expressions cannot read the clock, every decision can be reproduced from
history.

### Events

`WorkflowCreated`, `WorkflowStarted`, `StepScheduled`, `StepSkipped`,
`StepFanOut`, `StepDispatched`, `StepSucceeded`, `StepAttemptFailed`, `StepAttemptUncertain`,
`RetryScheduled`, `StepFailed`, `StepCancelled`, `TimerStarted`, `TimerFired`,
`StaleResultIgnored`, `CancelRequested`, `WorkflowTimeoutReached`,
`CompensationStarted`, `CompensationCompleted`, `CompensationFailed`,
`WorkflowCompleted`, `WorkflowFailed`, `WorkflowCancelled`, `WorkflowTimedOut`.

`orbit logs <id>` prints them as a timeline. `orbit logs <id> --json` prints
the raw events, with sensitive-looking fields redacted.

## Lifecycles

Workflow: `RUNNING → COMPLETED`, or `RUNNING → COMPENSATING → FAILED | CANCELLED | TIMED_OUT`.
The final record carries `compensation: none | completed | failed`.

Step (unit): `PENDING → SCHEDULED → DISPATCHED → SUCCEEDED`, with
`RETRY_WAIT`, `SKIPPED`, `FAILED`, `CANCELLED`, for sleeps `WAITING`, and for
fanned-out steps `RUNNING`. `ATTEMPT_FAILED` and `UNCERTAIN` are transient
states that the decider resolves right away.

Unit keys: `step`, `step[i]` for element `i` of a `for` step, and
`compensate:<unit>` for compensations (e.g. `compensate:reserve[2]`).

## Fan-out and child workflows

A `for` step, once eligible, evaluates its list once and records
`StepFanOut { items }`. Each element unit is then scheduled like an ordinary
step, at most `max_parallel` at a time, with the element bound to the loop
variable. When all elements succeed, the step succeeds with the list of their
outputs, in order. When one fails permanently, the step fails. Siblings still
in flight are awaited, and the abort path compensates each element that
succeeded, newest first.

A `workflow` step is dispatched by the runtime itself. It starts the child
execution with the deterministic id `<parent>.<unit>` (dots replace brackets),
records the link in the child's `WorkflowCreated { parent }`, and waits. Every
tick reconciles parents and children: a finished child's result (or error)
becomes the parent unit's result, and a child whose parent is aborting, or no
longer waits for it (for example after a timeout), is cancelled. Because the
id is deterministic and reconciliation runs continuously, a crash at any point
neither duplicates nor loses a child.

## Scheduling

A step becomes eligible when every dependency is `SUCCEEDED` or `SKIPPED`, the
workflow is not being aborted, and its `when` condition is true. Its input and
idempotency key are evaluated **once**, when it is scheduled, and stored in
`StepScheduled`. Retries and redeliveries reuse them unchanged.

Independent steps are scheduled together and run concurrently on whatever
workers are available. Workers receive the oldest matching queued task. Matching
requires the same task name, the same contract version, and the same runtime
when the step says `using <language>`.

## Retries and timeouts

* `attempts` is the total number of attempts, including the first.
* Backoff: `fixed` waits `initial_delay` each time. `exponential` waits
  `initial_delay × 2^(n−1)`, capped at `max_delay`.
* An attempt is retried when the error is retryable, its code is not in
  `non_retryable`, and attempts remain. Runtime-generated failures that are
  never retried: `OUTPUT_CONTRACT_VIOLATION`, `INPUT_CONTRACT_VIOLATION`,
  `EXPRESSION_ERROR`, `QUEUE_TIMEOUT`, `ADAPTER_DISABLED`.
* `timeout` limits one attempt, from dispatch to result (`TIMEOUT`, retryable).
  `queue_timeout` limits how long a task may wait for a worker. Workflow
  `timeout` limits the whole execution.
* Timeouts are cooperative. The worker is told to stop, through the cancel list
  in heartbeat responses (`ctx.cancelled` / `ctx.signal`), but the runtime
  cannot guarantee that an external operation has stopped.

## Delivery guarantees and idempotency

ORBIT delivers tasks **at least once**. It never promises exactly-once execution
of external side effects.

* Every task carries an `idempotency_key`. By default this is
  `<execution_id>/<step>`; set your own with `idempotency_key:`. The key is
  identical across retries and redeliveries, so workers should use it to make
  side effects idempotent.
* **Uncertain outcomes are never assumed to have failed.** A dispatched attempt
  becomes `UNCERTAIN` when its worker session goes silent for longer than the
  session timeout, the worker unregisters, or the runtime restarts and the
  worker never reports back. The *same attempt* is then redelivered
  (`redelivery: true`, same attempt number, same key), and redelivery does not
  use up a retry.
* **Reconciliation:** a result for *any* token issued for an unresolved step is
  accepted, because the work really happened. A late success from an attempt
  that already timed out still completes the step. Results that arrive after a
  step is resolved are recorded as `StaleResultIgnored` and have no effect.
* Failures are only accepted for the attempt currently in flight.

## Compensation

When a step fails for good, or the workflow is cancelled or times out:

1. Work that has not started is withdrawn (`StepCancelled`).
2. The runtime waits for in-flight attempts, which may still succeed and so
   need compensating.
3. Every succeeded step that has a `compensate` block is compensated **one at
   a time, newest first**. Compensations have their own retry policy and
   timeouts.
4. The workflow ends `FAILED`, `CANCELLED` or `TIMED_OUT` with
   `compensation: completed`. If a compensation itself fails, the workflow ends
   with `compensation: failed` and the history shows exactly what was and was
   not undone.

## Crash recovery

On startup the runtime:

1. loads the workflow definitions;
2. loads every execution that is not yet terminal and replays its history;
3. re-indexes the tokens of in-flight attempts;
4. runs the decider, which fires overdue timers and retries and schedules
   ready steps;
5. waits one session timeout for workers to reconnect and report in-flight
   results. The SDKs keep retrying result delivery through outages. After that
   window, attempts nobody reported are redelivered (see above).

The end-to-end test `test_z_crash_recovery` kills the runtime with `SIGKILL`
while a payment is being charged. It checks that the execution completes after
a restart and that exactly one success was recorded for the charge.

## Versioning and immutability

Definitions are keyed by `(name, version)` and carry a SHA-256
`definition_hash` of their semantic content. Source locations are excluded, so
reformatting a file does not change the hash. Registering identical content is
a no-op, and different content under an existing version is rejected with
`DEFINITION_CONFLICT`. Executions are pinned to the hash they started with.

Starting an execution with a client-chosen `execution_id` is idempotent. A
repeated start returns the existing execution instead of creating a duplicate.

## Security

* `--token` (or `ORBIT_TOKEN`) requires `Authorization: Bearer <token>` on every
  endpoint except `/v1/health`. The comparison is constant-time.
* `--worker-token` (or `ORBIT_WORKER_TOKEN`) adds a second, restricted
  credential for worker hosts. It can only call `/v1/workers/*` and
  `/v1/tasks/*`; anything else returns `403 FORBIDDEN`. A compromised worker
  host therefore cannot start, cancel or read executions. It requires
  `--token` and must differ from it. The SDKs prefer `ORBIT_WORKER_TOKEN` over
  `ORBIT_TOKEN`.
* `--tls-cert cert.pem --tls-key key.pem` serves HTTPS directly (rustls, TLS
  1.2+). Clients trust a private CA with `--ca-cert` / `ORBIT_CA_CERT` (CLI,
  Python, Go, Rust) or `NODE_EXTRA_CA_CERTS` (Node.js). The CA certificate
  must be a real CA certificate (`basicConstraints=CA:TRUE`, `keyCertSign`),
  not the server certificate itself.
* **Per-worker identities** (`--workers-file workers.toml`) give each worker its
  own credential and limits:

  ```toml
  [[worker]]
  id = "payments-1"
  token_sha256 = "…"          # `orbit token --worker payments-1 --task 'payments.*'` prints a token and this hash
  tasks = ["payments.*"]      # exact names, `prefix.*` or `*`
  runtime = "python"          # optional

  [[worker]]
  id = "inventory"
  client_cert = "inventory.workers.internal"   # mutual TLS: matches a DNS SAN or the subject CN
  tasks = ["inventory.reserve", "inventory.release"]
  ```

  A worker identity can register only under its own `id`, announce only tasks
  its patterns allow, and use only its own sessions and task tokens. A token
  issued to another worker is refused with `403`. Only token *hashes* are
  stored. The workers file requires `--token`.
* **Mutual TLS** (`--tls-client-ca ca.pem`, together with `--tls-cert`/`--tls-key`):
  client certificates signed by that CA identify workers declared with
  `client_cert`. A valid certificate that matches no worker is refused.
  Clients without a certificate can still authenticate with a token, which is
  how the admin CLI connects. Every SDK supports client certificates through
  `ORBIT_CLIENT_CERT` / `ORBIT_CLIENT_KEY`, and the CLI through `--client-cert`
  / `--client-key`.
* The server refuses to bind a non-loopback address without a token, unless you
  pass `--insecure`.
* Request bodies are capped (`--max-body`, 4 MiB by default). Execution input is
  validated strictly and worker output is validated against its contract.
* No shell execution exists. The HTTP adapter is off by default and only allows
  `http(s)` URLs.
* `orbit logs/inspect` redact values whose keys look sensitive (`password`,
  `secret`, `api_key`, `authorization`, ...). Runtime logs never include
  payloads.
* Registering is not the same as being trusted. Results are validated, stale
  tokens are ignored, and workers cannot touch workflow state except through
  results for tokens issued to them.

Not yet implemented: capability-based permissions beyond task patterns, and
per-execution resource limits.

## Observability

* Structured logs: `orbit server --log-format json`, with the level set by
  `ORBIT_LOG` (e.g. `ORBIT_LOG=orbit_runtime=debug` logs every event).
* Prometheus metrics at `/v1/metrics`.
* **OpenTelemetry traces** (`--otlp-endpoint http://collector:4318`, or
  `OTEL_EXPORTER_OTLP_ENDPOINT`; `OTEL_EXPORTER_OTLP_HEADERS` and
  `OTEL_SERVICE_NAME` are honoured). They are exported over OTLP/HTTP with
  JSON encoding to any OpenTelemetry Collector, Jaeger, Tempo or Honeycomb
  endpoint:
  - one trace per execution, with a root span `workflow <Name>`;
  - one span per task attempt, timer and compensation, with execution, unit,
    attempt, worker, redelivery and outcome attributes, and failed attempts
    marked as errors;
  - child workflows join their parent's trace, under the step that started them.

  Ids are derived deterministically, so restarts never duplicate spans. Export
  happens on a background thread after events are durable, and never blocks
  the engine.
* `orbit status`, `orbit logs`, `orbit inspect`, `orbit ps` and
  `orbit workers list`.

## Worker protocol bindings

The worker protocol (`protocol/orbit/worker/v1/worker.proto`) is served over
HTTP/JSON on the API port, and over **gRPC** when `--grpc-addr` is set (e.g.
`--grpc-addr 127.0.0.1:7234`). Both bindings share the engine, the
credentials (`authorization: Bearer` metadata for gRPC), per-worker identities,
TLS and mutual TLS. gRPC errors carry the ORBIT error code in the
`orbit-error-code` metadata entry. See [`protocol/README.md`](../protocol/README.md).

## Server options

| Option | Environment | Purpose |
|---|---|---|
| `--db` | `ORBIT_DB` | SQLite path or PostgreSQL URL |
| `--addr` | `ORBIT_ADDR` | HTTP API address (default `127.0.0.1:7233`) |
| `--grpc-addr` | `ORBIT_GRPC_ADDR` | also serve the gRPC worker protocol |
| `--token` | `ORBIT_TOKEN` | admin bearer token |
| `--worker-token` | `ORBIT_WORKER_TOKEN` | shared worker credential (worker endpoints only) |
| `--workers-file` | `ORBIT_WORKERS_FILE` | per-worker identities |
| `--tls-cert`, `--tls-key` | `ORBIT_TLS_CERT`, `ORBIT_TLS_KEY` | serve HTTPS (and TLS for gRPC) |
| `--tls-client-ca` | `ORBIT_TLS_CLIENT_CA` | mutual TLS |
| `--otlp-endpoint` | `OTEL_EXPORTER_OTLP_ENDPOINT` | OpenTelemetry trace export |
| `--enable-http-adapter` | | allow `orbit.http.request` steps |
| `--session-timeout` | | seconds before a silent worker is considered lost (default 30) |
| `--max-body` | | request size limit in bytes (default 4 MiB) |
| `--log-format` | | `text` or `json` |
| `--insecure` | | allow a non-loopback address without a token |

## HTTP API

| Method & path | Purpose |
|---|---|
| `GET /v1/health` | liveness, versions (no auth) |
| `GET /v1/metrics` | Prometheus metrics |
| `POST /v1/workflows` | register compiled IR (201 created, 200 unchanged, 409 conflict) |
| `GET /v1/workflows` / `GET /v1/workflows/{name}[/{version}]` | definitions |
| `POST /v1/executions` | `{workflow, version?, input, execution_id?}` → `{execution_id, created}` |
| `GET /v1/executions?status=&workflow=&limit=` | list |
| `GET /v1/executions/{id}` | current state |
| `GET /v1/executions/{id}/history` | event history |
| `GET /v1/executions/{id}/wait?timeout_ms=` | long-poll until terminal |
| `POST /v1/executions/{id}/cancel` | `{reason?}` |
| `GET /v1/workers` | registered workers |
| worker protocol | see [`protocol/README.md`](../protocol/README.md) |

Errors always have the form `{"error": {"code": "...", "message": "..."}}`, and
responses carry `x-orbit-api-version: orbit-api/1`.
EOF