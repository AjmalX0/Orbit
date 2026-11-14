# Runtime semantics (v0.1)

The ORBIT runtime (`orbit server`) is one process that owns all workflow state.
Workers are separate processes in any language. They never own workflow state.
They pull tasks, run business logic, and report results.

```text
            orbit CLI / apps ──HTTP──┐
                                     ▼
 ┌──────────────────── orbit server ─────────────────────┐
 │  HTTP API ─► Engine ─► decide(state, IR, now) (pure)   │
 │                │  ▲                                    │
 │                ▼  │ fold(events)                       │
 │          SQLite: execution_events (authoritative)      │
 └────────────▲───────────────────────────────▲──────────┘
     long-poll│/results                       │
      Python worker                    TypeScript worker   ...
```

## Event sourcing

Every state change is an **event** appended to `execution_events`. The current
state of an execution is the left fold of its events (`ExecutionState::replay`).
The `workflow_executions` table is only a projection for listing. It is updated
in the same SQLite transaction as the events, using WAL mode with
`synchronous=FULL`.

The engine never mutates memory before the transaction commits. If the write
fails, the in-memory state is unchanged.

Decisions come from a **pure decider**, `decide(state, definition, now)`. It
returns the events that should happen next. The engine runs it to a fixpoint
and commits the result as one batch. Because the decider does no I/O and
expressions cannot read the clock, every decision can be reproduced from
history.

### Events

`WorkflowCreated`, `WorkflowStarted`, `StepScheduled`, `StepSkipped`,
`StepDispatched`, `StepSucceeded`, `StepAttemptFailed`, `StepAttemptUncertain`,
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
`RETRY_WAIT`, `SKIPPED`, `FAILED`, `CANCELLED`, and for sleeps `WAITING`.
`ATTEMPT_FAILED` and `UNCERTAIN` are transient states that the decider resolves
right away. Compensations are units named `compensate:<step>`.

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

Not yet implemented (see the roadmap): TLS termination (put a reverse proxy in
front for now), per-worker credentials, and capability-based permissions.

## Observability

* Structured logs: `orbit server --log-format json`, with the level set by
  `ORBIT_LOG` (e.g. `ORBIT_LOG=orbit_runtime=debug` logs every event).
* Prometheus metrics at `/v1/metrics`.
* `orbit status`, `orbit logs`, `orbit inspect`, `orbit ps` and
  `orbit workers list`.

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