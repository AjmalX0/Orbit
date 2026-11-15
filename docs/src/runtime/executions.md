# Executions and history

An execution is one run of a workflow definition with one input. The runtime records everything that happens to it as an append-only history of events, and derives its current state, and the state of each of its steps, from that history. This page describes the execution lifecycle, every execution and unit status, every history event with its fields, how the CLI shows them, and how cancellation and timeouts behave.

## Event sourcing

Every state change is an event appended to the `execution_events` table. The current state of an execution is the left fold of its events: replaying the history from the first event always produces the same state. A projection table, `workflow_executions`, is updated in the same transaction for listing, but the history is authoritative.

Decisions come from a pure function, `decide(state, definition, now)`, that returns the events that should happen next: schedule ready steps, fan out `for` steps, evaluate `compute` steps, fire timers, deliver buffered signals to `await` steps, apply retry policies, start compensation, finish the workflow. The engine runs it until it returns nothing and commits the result as one batch. Because the decider performs no I/O and expressions cannot read the clock, every decision can be explained from the history.

## Starting an execution

You start an execution with `orbit run`, `POST /v1/executions`, or a parent workflow's `workflow "Name"` step.

- The definition is chosen by name and optional version; without a version, the highest registered version (by semantic version order) is used. The execution is pinned to that definition's hash for its whole life.
- Input defaults are applied and the input is validated strictly against the workflow's `input` type. An invalid input is rejected with `INVALID_INPUT` and no execution is created.
- The execution id is a random UUID unless you pass one. A client-chosen id makes starting idempotent: starting the same id again returns the existing execution (`created: false`) instead of creating a second one. A child workflow's id is `<parent id>.<unit>`, with dots in place of brackets (`order-7.ship`, `order-7.reserve.2`); a child started by run 2 or later of its parent ([continue-as-new](#runs-continue-as-new)) is `<parent id>.run<N>.<unit>`.
- A [schedule](schedules.md) starts executions with the id `<schedule id>:<occurrence>`, for example `nightly-orders:2026-11-16T01:00:00Z`, and records `schedule_id` in the execution.

```console
$ orbit run examples/order_processing.orbit --id order-1001 --no-wait --input-file input.json
```

## Execution statuses

| Status | Terminal | Meaning |
|---|---|---|
| `RUNNING` | no | Steps are being scheduled and run. An execution that is being aborted (after a failure, a cancellation or a timeout) stays `RUNNING` while it withdraws queued work and waits for in-flight attempts. |
| `COMPENSATING` | no | Compensations are running, one at a time, newest first. |
| `COMPLETED` | yes | Every step finished and the `return` expression produced a valid output. |
| `FAILED` | yes | A step failed permanently, a [resource limit](server.md#resource-limits) was reached, the `return` expression failed, or the output did not match the workflow's `output` type. |
| `CANCELLED` | yes | Cancellation was requested and the abort finished. |
| `TIMED_OUT` | yes | The workflow `timeout` passed and the abort finished. |
| `CONTINUED_AS_NEW` | yes, for the run | The run ended with `continue_as_new` and the next run of the execution started. Only archived runs have this status (`GET /v1/executions/{id}/history?run=N`); the execution itself is the latest run. |

```text
                  ┌──────────────► COMPLETED
                  │
RUNNING ──────────┼──────────────► FAILED | CANCELLED | TIMED_OUT     (nothing to compensate)
                  │
                  └─► COMPENSATING ─► FAILED | CANCELLED | TIMED_OUT
```

A terminal execution also carries `compensation`:

| Value | Meaning |
|---|---|
| `none` | Nothing needed compensating. |
| `completed` | Every compensation succeeded. |
| `failed` | A compensation failed permanently. The history shows exactly what was and was not undone. |

## Runs (continue-as-new)

An execution has one or more runs. It starts with run 1; every
[`continue_as_new`](../language/continue-as-new.md) ends the current run and
starts run N+1 under the same execution id, with the new input and an empty
history. In one transaction the runtime appends `WorkflowContinuedAsNew` to the
finished run, moves that run's events from `execution_events` to
`execution_run_events` (keyed by execution id and run), and writes the new
run's first events: `WorkflowCreated` (with `run`), `WorkflowStarted`, and one
`SignalReceived` for every signal the finished run received but no step took,
in arrival order. A crash cannot leave an execution between two runs.

- `GET /v1/executions/{id}`, `orbit status` and `orbit inspect` show the latest run; its state has `run` (absent for run 1).
- `GET /v1/executions/{id}/history` returns the latest run's history; `?run=N` (and `orbit logs --run N`, `orbit replay --run N`) an earlier run's. Every run's `seq` starts at 1.
- `wait` returns when the latest run ends with a terminal status other than `CONTINUED_AS_NEW`.
- Signals go to the current run. A signal sent while a run is finishing is either recorded in that run (and carried over if no step took it) or in the next one; none is lost.
- Queries, cancellation and the workflow `timeout` apply to the current run. The `timeout` counts from the run's start, and `created_at_ms` is the run's creation time.
- The workflow version does not change: every run uses the definition the execution started with.
- A new run is not a new execution: it does not count against `--max-active-executions` and does not increment `orbit_executions_started_total`. `--max-history-events` and the workflow's `max_history_events` apply to each run.
- The default idempotency key of a unit in run N > 1 is `<execution id>/run<N>/<unit>`, so a worker that deduplicates by key does not mistake a later run's step for an earlier one's.

`GET /v1/executions` lists the execution once.

## Units

The runtime schedules units. A unit is one of:

| Unit key | What it is |
|---|---|
| `step` | A step. |
| `step[i]` | Element `i` (from 0) of a `for` step. |
| `compensate:<unit>` | The compensation of a unit, for example `compensate:reserve_inventory` or `compensate:reserve[2]`. |
| `on_<kind>:<step>` | A step of an outcome handler, for example `on_failure:tell_customer` or `on_cancel:confirm_cancel`. See [Outcome handlers](../language/handlers.md). |

Each unit has a status, an attempt number, its evaluated input and idempotency key, its output or error, and the times it was scheduled, dispatched and finished. A unit's input and idempotency key are evaluated once, when it is first scheduled, and reused unchanged for retries and redeliveries.

### Unit statuses

| Status | Terminal | Meaning |
|---|---|---|
| `PENDING` | no | Not started: waiting for dependencies, or not reached. A pending unit has no `status` field in the API. |
| `SCHEDULED` | no | Queued for a worker. Its input is evaluated and stored. |
| `DISPATCHED` | no | A worker received it and holds a token for it. For a child workflow step: the child execution is running. |
| `ATTEMPT_FAILED` | no | Transient: an attempt failed and the decider is choosing between a retry and failure. Never observed between commits. |
| `UNCERTAIN` | no | Transient: the outcome of the attempt is unknown and it will be redelivered. Never observed between commits. |
| `RETRY_WAIT` | no | Waiting for the backoff delay before the next attempt. |
| `WAITING` | no | A `sleep` timer is running, or an `await` step is waiting for a signal. |
| `RUNNING` | no | A `for` step whose elements are running. |
| `SUCCEEDED` | yes | Finished with an output. |
| `SKIPPED` | yes | Its `when` condition was false (or `null`). Its output is `null`. |
| `FAILED` | yes | Failed permanently: retries exhausted, a non-retryable error, or an error the runtime never retries. |
| `CANCELLED` | yes | Withdrawn before it started because the execution is being aborted. |

A step becomes eligible when every dependency is `SUCCEEDED` or `SKIPPED`, the execution is not being aborted, and its `when` condition is true. Independent steps are scheduled together and run concurrently.

```text
PENDING ─► SCHEDULED ─► DISPATCHED ─► SUCCEEDED
   │           │             │
   │           │             ├─► ATTEMPT_FAILED ─► RETRY_WAIT ─► SCHEDULED
   │           │             │                  └► FAILED
   │           │             └─► UNCERTAIN ─► SCHEDULED (redelivery)
   │           └─► CANCELLED
   ├─► SKIPPED
   ├─► SUCCEEDED                       (compute)
   ├─► WAITING ─► SUCCEEDED | FAILED   (sleep, await)
   └─► RUNNING ─► SUCCEEDED | FAILED   (for)
```

## History events

Every event has these fields:

| Field | Type | Meaning |
|---|---|---|
| `seq` | integer | Position in the history, starting at 1, without gaps. |
| `timestamp_ms` | integer | Commit time, milliseconds since the Unix epoch. All events of one batch share it. |
| `type` | string | Event type, below. |

Error objects (`error`) have the fields `code` (string), `message` (string), `retryable` (boolean) and optionally `details` (any JSON). Events that refer to a unit carry its key in `step`.

### Workflow events

| Event | Fields | Meaning |
|---|---|---|
| `WorkflowCreated` | `workflow`, `version`, `definition_hash`, `input`, `parent`?, `schedule_id`?, `run`? | The execution (or, with `run`, run `run` of it) exists. `input` has defaults applied. `parent` (`{"execution_id", "step"}`) is set for child workflows, `schedule_id` for executions a [schedule](schedules.md) started, and `run` for runs after the first. Always event 1. |
| `WorkflowStarted` | none | The execution began. Always event 2. |
| `WorkflowTimeoutReached` | none | The workflow `timeout` passed. The execution starts aborting. |
| `CancelRequested` | `reason` | Cancellation was requested. The execution starts aborting, unless it is already aborting. |
| `WorkflowCompleted` | `output` | Terminal: `COMPLETED`. |
| `LimitExceeded` | `limit`, `message` | A server resource limit was reached; `limit` is `max_history_events`. The execution starts aborting with the error `LIMIT_EXCEEDED` and `message`. See [Resource limits](server.md#resource-limits). |
| `WorkflowFailed` | `error`, `compensation` | Terminal: `FAILED`. The error message names the failed step. |
| `WorkflowCancelled` | `compensation` | Terminal: `CANCELLED`. |
| `WorkflowTimedOut` | `compensation` | Terminal: `TIMED_OUT`. |
| `WorkflowContinuedAsNew` | `next_run`, `input` | Terminal for the run: `CONTINUED_AS_NEW`. Run `next_run` starts with `input` (defaults applied). The last event of an archived run. |

### Step events

| Event | Fields | Meaning |
|---|---|---|
| `StepScheduled` | `step`, `attempt`, `input`, `idempotency_key`, `redelivery` | The unit is queued with its evaluated input. Recorded for the first attempt, for each retry (same input and key, next attempt number) and for each redelivery (same attempt number, `redelivery: true`). |
| `StepSkipped` | `step`, `reason` | The `when` condition was false. |
| `StepFanOut` | `step`, `items` | A `for` step evaluated its list once. One unit per element follows, `step[0]`, `step[1]`, and so on. |
| `StepDispatched` | `step`, `attempt`, `token`, `worker_id`, `worker_session`, `deadline_ms` | A worker received the unit. `deadline_ms` is `null` without a `timeout`. For child workflows, `worker_id` is `orbit-runtime` and `worker_session` is `child:<child id>`. |
| `StepSucceeded` | `step`, `attempt`, `output` | The unit finished. For a `for` step, `output` is the list of element outputs, in order. |
| `StepAttemptFailed` | `step`, `attempt`, `error` | One attempt failed: a worker reported a failure, the attempt exceeded its `timeout` (`TIMEOUT`), it was not picked up within `queue_timeout` (`QUEUE_TIMEOUT`), its output violated the contract (`OUTPUT_CONTRACT_VIOLATION`), or a built-in task failed. |
| `StepAttemptUncertain` | `step`, `attempt`, `reason` | The outcome of a dispatched attempt is unknown: the worker stopped responding, unregistered, or the runtime restarted while a built-in task ran. The same attempt is redelivered. |
| `RetryScheduled` | `step`, `next_attempt`, `delay_ms`, `fire_at_ms` | The failed attempt will be retried after `delay_ms`. |
| `StepFailed` | `step`, `error` | The unit failed permanently. For a regular step, the execution starts aborting. When retries were exhausted, the message ends with `(gave up after N attempts)`. |
| `StepCancelled` | `step` | A unit that had not started was withdrawn because the execution is aborting. |
| `TimerStarted` | `step`, `fire_at_ms` | A `sleep` step started its durable timer. |
| `TimerFired` | `step` | The timer fired. The `sleep` step succeeds with `{}`. |
| `StaleResultIgnored` | `step`, `token`, `outcome` | A result arrived that no longer matters. `outcome` is `duplicate` (the unit was already resolved) or `failure` (a failure for an attempt that is not the one in flight). It has no effect on the execution. |

### Signal events

| Event | Fields | Meaning |
|---|---|---|
| `SignalReceived` | `name`, `payload` | A signal was sent with `POST /v1/executions/{id}/signals/{name}` (or `orbit signal`), validated against the declaration and buffered. `payload` has defaults applied. The event's `seq` identifies the signal. |
| `SignalAwaitStarted` | `step`, `signal`, `deadline_ms`? | An `await` step started waiting; the unit is `WAITING`. `deadline_ms` is set when the step has a `timeout`. |
| `SignalDelivered` | `step`, `signal_seq` | The `await` unit took the buffered signal recorded at `seq` `signal_seq`, and succeeded with its payload as output. |

An `await` step whose `timeout` passes is recorded as `StepFailed` with `SIGNAL_TIMEOUT`; one that is withdrawn during an abort as `StepCancelled`. A `compute` step is recorded as a single `StepSucceeded` (or `StepFailed` with `EXPRESSION_ERROR`), without `StepScheduled` or `StepDispatched`. Evaluating a query records nothing. See [Signals and queries](../language/signals.md) and [Compute steps](../language/compute.md).

```json
{"seq":4,"timestamp_ms":1794746714132,"type":"SignalAwaitStarted","step":"decision","signal":"review","deadline_ms":1794919514132}
{"seq":5,"timestamp_ms":1794746714669,"type":"SignalReceived","name":"review","payload":{"approved":true,"reviewer":"dana","comment":"verified by phone"}}
{"seq":6,"timestamp_ms":1794746714671,"type":"SignalDelivered","step":"decision","signal_seq":5}
```

### The signal buffer

Every received signal is kept in the execution state, in the `signals` array of `GET /v1/executions/{id}` and `orbit inspect <id>`, in arrival order. A signal is buffered until an `await` step of the same name takes it; each step takes the oldest undelivered signal of its name, and each signal is delivered at most once. A signal that no step takes stays in the buffer without `delivered_to`.

| Field | Type | Meaning |
|---|---|---|
| `seq` | integer | Sequence number of its `SignalReceived` event. |
| `name` | string | Signal name. |
| `payload` | object | Validated payload. |
| `received_at_ms` | integer | When it was recorded. |
| `delivered_to` | string, optional | The unit that took it. |

```json
"signals": [
  {"seq": 5, "name": "review", "payload": {"approved": true, "reviewer": "dana", "comment": "verified by phone"},
   "received_at_ms": 1794746714669, "delivered_to": "decision"}
]
```

The field is absent for executions that never received a signal.

### Compensation events

| Event | Fields | Meaning |
|---|---|---|
| `CompensationStarted` | `reason`, `steps` | The execution is `COMPENSATING`. `steps` lists the units to compensate, newest success first. `reason` explains the abort. |
| `CompensationCompleted` | none | Every compensation succeeded. Followed by the terminal event. |
| `CompensationFailed` | `step`, `error` | A compensation failed permanently. `step` is the compensated unit. Recorded once per failed compensation after the remaining compensations have run, followed by the terminal event with `compensation: failed`. |

Compensation units use the step events above with keys such as `compensate:reserve_inventory`.

### Handler events

| Event | Fields | Meaning |
|---|---|---|
| `HandlersStarted` | `kind`, `compensation` | Compensation is finished (or there was none) and the steps of the `on <kind>` handler start: `kind` is `failure`, `cancel` or `timeout`. `compensation` (`none`, `completed` or `failed`) is the outcome the terminal event will carry. Recorded only when the workflow declares a handler of that kind. |

Handler steps use the step events above with keys such as `on_failure:tell_customer`. While they run, the execution keeps its status (`RUNNING` or `COMPENSATING`). From `HandlersStarted` on, the execution state (`GET /v1/executions/{id}`) has a `handlers` field, for example `{"kind": "failure", "compensation": "none"}`; it stays after the terminal event and is absent for executions that ran no handler. When every handler step has finished, the terminal event is recorded with the original error. A handler step that fails does not change the execution's outcome. See [Outcome handlers](../language/handlers.md).

```json
{"seq":13,"timestamp_ms":1794754619132,"type":"LimitExceeded","limit":"max_history_events","message":"the execution reached 10 history events (--max-history-events or the workflow's `limits`)"}
{"seq":15,"timestamp_ms":1794754619132,"type":"HandlersStarted","kind":"failure","compensation":"none"}
{"seq":17,"timestamp_ms":1794754619132,"type":"WorkflowFailed","error":{"code":"LIMIT_EXCEEDED","message":"the execution reached 10 history events (--max-history-events or the workflow's `limits`)","retryable":false},"compensation":"none"}
```

### Runtime error codes

Errors that the runtime itself produces use these codes:

| Code | Retryable | Produced when |
|---|---|---|
| `TIMEOUT` | yes | An attempt exceeded its step `timeout`. |
| `QUEUE_TIMEOUT` | no | No worker picked up the task within `queue_timeout`. |
| `EXPRESSION_ERROR` | no | Evaluating `with`, `when`, `for`, `idempotency_key`, a `compute` step or `return` failed, or `when` did not produce a `Bool`. |
| `SIGNAL_TIMEOUT` | no | An `await` step's `timeout` passed before a signal of its name arrived. |
| `INPUT_CONTRACT_VIOLATION` | no | The evaluated step input does not match the task's input contract. |
| `OUTPUT_CONTRACT_VIOLATION` | no | A worker's output does not match the task's output contract, or the workflow output does not match its `output` type. |
| `ADAPTER_DISABLED` | no | An `orbit.http.request` step ran on a server without `--enable-http-adapter`. |
| `HTTP_ERROR` | depends | The HTTP adapter failed: a missing or non-HTTP URL, a connection error, or an error response. |
| `CHILD_NOT_REGISTERED`, `CHILD_START_FAILED`, `CHILD_MISSING` | no | A child workflow could not be started or found. |
| `CHILD_CANCELLED`, `CHILD_TIMED_OUT` | no | A child workflow ended `CANCELLED` or `TIMED_OUT`. A child that ends `FAILED` passes its own error code up, with the child's id in `details.child_execution_id`. |
| `CANCELLED` | no | A retry that was waiting when the execution started aborting, with no earlier error to report. |
| `LIMIT_EXCEEDED` | no | A `for` step's list is longer than `--max-fan-out` (on `StepFailed`), or the history reached `--max-history-events` (on `WorkflowFailed`, after `LimitExceeded`). See [Resource limits](server.md#resource-limits). |

A worker's own error codes are passed through unchanged. Whether a worker error is retried depends on its `retryable` flag and the step's `non_retryable` list; see [Retries and timeouts](../language/retries-timeouts.md).

## Example history

The execution `order-1002` ran [`examples/order_processing.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/order_processing.orbit). The first risk check failed with a retryable error and was retried; the worker reported the validation result twice; the payment was declined with a non-retryable error, so the reserved inventory was released:

```console
$ orbit logs order-1002
+     0ms   #1  WorkflowCreated
+     0ms   #2  WorkflowStarted
+     1ms   #3  StepScheduled             step=validate_order attempt=1
+     1ms   #4  StepScheduled             step=analyze_risk attempt=1
+    11ms   #5  StepDispatched            step=validate_order attempt=1 worker_id=ts-1
+    14ms   #6  StepSucceeded             step=validate_order attempt=1
+    15ms   #7  StepScheduled             step=reserve_inventory attempt=1
+    17ms   #8  StaleResultIgnored        step=validate_order outcome=duplicate
+    21ms   #9  StepDispatched            step=analyze_risk attempt=1 worker_id=py-1
+    23ms  #10  StepAttemptFailed         step=analyze_risk attempt=1 error=RISK_UNAVAILABLE scoring service returned 503
+    24ms  #11  RetryScheduled            step=analyze_risk delay_ms=1000
+    33ms  #12  StepDispatched            step=reserve_inventory attempt=1 worker_id=ts-1
+    35ms  #13  StepSucceeded             step=reserve_inventory attempt=1
+    1.0s  #14  StepScheduled             step=analyze_risk attempt=2
+    1.3s  #15  StepDispatched            step=analyze_risk attempt=2 worker_id=py-1
+    1.3s  #16  StepSucceeded             step=analyze_risk attempt=2
+    1.3s  #17  StepScheduled             step=process_payment attempt=1
+    1.6s  #18  StepDispatched            step=process_payment attempt=1 worker_id=py-1
+    1.6s  #19  StepAttemptFailed         step=process_payment attempt=1 error=CARD_DECLINED card was declined
+    1.6s  #20  StepFailed                step=process_payment error=CARD_DECLINED card was declined
+    1.6s  #21  StepCancelled             step=send_notification
+    1.6s  #22  CompensationStarted       reason=step `process_payment` failed: card was declined
+    1.6s  #23  StepScheduled             step=compensate:reserve_inventory attempt=1
+    1.6s  #24  StepDispatched            step=compensate:reserve_inventory attempt=1 worker_id=ts-1
+    1.6s  #25  StepSucceeded             step=compensate:reserve_inventory attempt=1
+    1.6s  #26  CompensationCompleted
+    1.6s  #27  WorkflowFailed            error=CARD_DECLINED step `process_payment` failed: card was declined
```

Some of the same events as raw JSON (`orbit logs order-1002 --json`):

```json
{"seq":11,"timestamp_ms":1794735367726,"type":"RetryScheduled","step":"analyze_risk","next_attempt":2,"delay_ms":1000,"fire_at_ms":1794735368726}
{"seq":17,"timestamp_ms":1794735368974,"type":"StepScheduled","step":"process_payment","attempt":1,"input":{"order_id":"1002","amount":99.5,"currency":"USD"},"idempotency_key":"charge-1002","redelivery":false}
{"seq":18,"timestamp_ms":1794735369286,"type":"StepDispatched","step":"process_payment","attempt":1,"token":"3d932a10-600d-4989-acc7-5234a3183b06","worker_id":"py-1","worker_session":"30e57815-e28e-4a2b-9c35-df96774668ae","deadline_ms":1794735399286}
{"seq":19,"timestamp_ms":1794735369289,"type":"StepAttemptFailed","step":"process_payment","attempt":1,"error":{"code":"CARD_DECLINED","message":"card was declined","retryable":false,"details":{"decline_code":"insufficient_funds"}}}
{"seq":22,"timestamp_ms":1794735369290,"type":"CompensationStarted","reason":"step `process_payment` failed: card was declined","steps":["reserve_inventory"]}
{"seq":27,"timestamp_ms":1794735369299,"type":"WorkflowFailed","error":{"code":"CARD_DECLINED","message":"step `process_payment` failed: card was declined","retryable":false,"details":{"decline_code":"insufficient_funds"}},"compensation":"completed"}
```

A durable timer and a workflow timeout (`timeout 3s`, a `sleep 200ms` step, and no worker for the next step):

```console
$ orbit logs timeout-1
+     0ms   #1  WorkflowCreated
+     0ms   #2  WorkflowStarted
+     1ms   #3  TimerStarted              step=pause
+   284ms   #4  TimerFired                step=pause
+   284ms   #5  StepScheduled             step=work attempt=1
+    3.1s   #6  WorkflowTimeoutReached
+    3.1s   #7  StepCancelled             step=work
+    3.1s   #8  WorkflowTimedOut
```

A redelivery after a worker went silent is shown in [Delivery guarantees](guarantees.md#uncertain-attempts-and-redelivery).

## Inspecting executions

| Command | Shows |
|---|---|
| `orbit ps [--status S] [--workflow W] [--limit N]` | Recent executions, newest first, with status and age. |
| `orbit status <id>` | The status, elapsed time, compensation outcome, error, a table of units (status, attempt, duration, error or worker) and the output. |
| `orbit logs <id>` | The history as a timeline. `--json` prints the raw events, one per line, redacted. |
| `orbit inspect <id>` | The full execution state as JSON, redacted. The same document as `GET /v1/executions/{id}`. |
| `orbit workers list` | Registered workers, whether they are online, and how many tasks they hold. |

```console
$ orbit status order-1001
COMPLETED order-1001  OrderProcessing@1.0.0
  elapsed: 1.6s

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  validate_order               SUCCEEDED          1      3ms  worker ts-1
  reserve_inventory            SUCCEEDED          1      3ms  worker ts-1
  analyze_risk                 SUCCEEDED          2      3ms  worker py-1
  process_payment              SUCCEEDED          1      3ms  worker py-1
  send_notification            SUCCEEDED          1      2ms  worker ts-1

  output: {"status":"completed","transaction_id":"tx-9","risk_score":0.12}
```

Units are listed in the order they last changed. `TIME` is the time from dispatch (or scheduling) to finish. Units that have not been scheduled yet are not listed. `orbit run` without `--no-wait` prints each unit transition as it happens and exits `0` only when the execution ends `COMPLETED`.

For automation, use the [HTTP API](../reference/http-api.md): `GET /v1/executions/{id}` for state, `/history` for events, and `/wait?timeout_ms=` to block until the execution is terminal.

## Cancellation

`orbit cancel <id> [--reason R]` or `POST /v1/executions/{id}/cancel` requests cancellation. The runtime records `CancelRequested` and aborts the execution:

1. Units that have not started are withdrawn (`StepCancelled`): pending units, queued units, running `sleep` timers, `await` steps waiting for a signal, and retries waiting for their backoff. A redelivery that is queued is not withdrawn, because its first delivery may have run.
2. Attempts in flight are awaited, because they may still succeed and then need compensating. Their workers see the tokens in the `cancel` list of their next heartbeat (`ctx.cancelled` or `ctx.signal` in the SDKs). Stopping is cooperative: the runtime cannot stop an external operation that is already running.
3. A child workflow whose parent is aborting is cancelled too, with the reason `parent workflow aborted`.
4. Every succeeded unit with a `compensate` block is compensated, one at a time, newest first. Compensations run even though the execution is aborting, with their own retry policy.
5. The execution ends `CANCELLED` with `compensation: none`, `completed` or `failed`.

Cancellation is idempotent while the execution runs: a second request, or a request during an abort caused by a failure or timeout, records nothing and returns the current state. The original abort reason wins: a failed execution that you cancel during compensation still ends `FAILED`. Cancelling a terminal execution returns `409 EXECUTION_FINISHED`.

```console
$ orbit cancel order-2001 --reason "customer changed their mind"
cancellation requested; execution is CANCELLED
```

## Timeouts

These timeouts apply, at different levels:

| Timeout | Declared on | Measures | On expiry |
|---|---|---|---|
| `timeout` | a step | One attempt, from dispatch to result. | The attempt fails with `TIMEOUT` (retryable); the step is retried if attempts remain. The worker sees the token in its heartbeat `cancel` list. A late success for that attempt is still accepted while the step is unresolved. |
| `queue_timeout` | a step | How long a scheduled task waits for a worker. Does not apply to redeliveries. | The attempt fails with `QUEUE_TIMEOUT` (not retried). |
| `timeout` | an `await` step | The wait for a signal, from `SignalAwaitStarted`. | The step fails with `SIGNAL_TIMEOUT` (not retried), and the execution aborts. |
| `timeout` | the workflow | The whole execution, from `WorkflowCreated`. | `WorkflowTimeoutReached`, then the abort described under cancellation, ending `TIMED_OUT`. |

The workflow timeout is checked only while the execution is `RUNNING` and not already aborting, so compensation is not cut short by the workflow timeout. A `workflow "Name"` step's `timeout` bounds the child execution; when it expires, the child is cancelled.

## See also

- [Delivery guarantees](guarantees.md)
- [Compensation](../language/compensation.md)
- [Outcome handlers](../language/handlers.md)
- [Running the server: resource limits](server.md#resource-limits)
- [Signals and queries](../language/signals.md)
- [Retries and timeouts](../language/retries-timeouts.md)
- [HTTP API](../reference/http-api.md)
- [CLI](../reference/cli.md)
