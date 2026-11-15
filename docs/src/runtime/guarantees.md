# Delivery guarantees

This page states what the ORBIT runtime guarantees about workflow state and task delivery, and what it does not. In short: committed history is durable and decides everything; tasks are delivered at least once, with a stable idempotency key; an attempt whose outcome is unknown is redelivered, never assumed to have failed; and a crash at any point loses no committed work. External side effects are not executed exactly once; your workers make them idempotent with the key the runtime provides.

## Durability: events before effects

Every state change is an event, and the engine follows one rule: write the events, then act on them.

- Events are appended in a database transaction, together with the update of the execution projection. Memory is updated only after the transaction commits. If the write fails, the in-memory state is unchanged and the request fails with `STORAGE_ERROR`.
- A task is handed to a worker only after its `StepDispatched` event is committed. A result is acknowledged to the worker (`{"accepted": true}`) only after its `StepSucceeded` or `StepAttemptFailed` event is committed.
- Traces are exported only after the events they describe are durable.
- SQLite runs in WAL mode with `synchronous=FULL`, so a committed event survives power loss. PostgreSQL commits each batch in one transaction.

As a consequence, whatever a client or worker was told is in the history, and whatever is not in the history did not happen as far as the runtime is concerned.

## At-least-once delivery

ORBIT delivers each task at least once. A task can be delivered more than once when:

- a worker received it but went silent before reporting (process crash, network partition, a pause longer than the session timeout);
- a worker unregistered while holding it;
- the runtime restarted and the worker did not report back within one session timeout;
- an attempt timed out and was retried while the first attempt was still running somewhere.

ORBIT does not promise exactly-once execution of external side effects. No orchestrator can: between an external system performing an effect and the result reaching the runtime, any failure leaves the outcome unknown.

## Idempotency keys

Every task carries an `idempotency_key`:

- The default is `<execution_id>/<unit>`, for example `order-1002/validate_order` or `order-1002/compensate:reserve_inventory`.
- Set your own with `idempotency_key:` on the step or the `compensate` block, for example `"charge-" + input.order_id`. The expression is evaluated once, when the unit is first scheduled.
- The key is identical across retries (`attempt` increases) and redeliveries (`redelivery: true`, same `attempt`).

Pass the key to the external system (most payment and messaging APIs accept one), or record it in your own store and skip work already done. Use a business key when the same effect must not happen twice even across executions, for example two executions started for the same order. See [Idempotency](../language/idempotency.md).

## Uncertain attempts and redelivery

A dispatched attempt becomes uncertain (`StepAttemptUncertain`) when:

| Cause | Reason recorded |
|---|---|
| Its worker session has not polled or heartbeated for longer than the session timeout. | ``worker `flaky-1` stopped responding`` |
| Its worker unregistered while holding it. | `worker unregistered` |
| The runtime restarted while a built-in task (the HTTP adapter) was running it. | `runtime restarted while the built-in task was running` |

An uncertain attempt is never treated as a failure. The same attempt is scheduled again (`StepScheduled` with `redelivery: true`, the same attempt number, input and key) and delivered to the next matching worker. A redelivery does not use up a retry, `queue_timeout` does not apply to it, and an abort does not withdraw it, because the first delivery may have run.

Here `flaky-1` received the task, then went silent; with `--session-timeout 5`, the attempt was redelivered to `flaky-2`:

```console
$ orbit logs u-1
+     0ms   #1  WorkflowCreated
+     0ms   #2  WorkflowStarted
+     1ms   #3  StepScheduled             step=work attempt=1
+    39ms   #4  StepDispatched            step=work attempt=1 worker_id=flaky-1
+    5.1s   #5  StepAttemptUncertain      step=work attempt=1 reason=worker `flaky-1` stopped responding
+    5.1s   #6  StepScheduled             step=work attempt=1 redelivery=true
+    6.6s   #7  StepDispatched            step=work attempt=1 worker_id=flaky-2
+    6.7s   #8  StepSucceeded             step=work attempt=1
+    6.7s   #9  WorkflowCompleted
```

The task `flaky-2` received:

```json
{"token":"5503f137-059e-455a-92c1-e65fa3690962","execution_id":"u-1","workflow":"Plain","workflow_version":"1.0.0",
 "step_id":"work","task_name":"demo.work","contract_version":"1.0.0","input":{"n":1},
 "idempotency_key":"u-1/work","attempt":1,"redelivery":true}
```

## Reconciliation

Results are reconciled against the history:

- **A success is accepted for any token issued for an unresolved unit.** The work really happened, so it counts, even if it comes from an attempt that timed out or was considered lost. In the example above, `flaky-1` came back and reported success with its original token before `flaky-2` finished; the runtime accepted it (event #8), and `flaky-2` found its own token in the `cancel` list of its next heartbeat.
- **A failure is accepted only for the attempt currently in flight.** A failure from an older attempt is recorded as `StaleResultIgnored` with `outcome: failure` and answered `{"accepted": false, "reason": "stale attempt"}`.
- **A result for a resolved unit has no effect.** It is recorded as `StaleResultIgnored` with `outcome: duplicate` and answered `{"accepted": false, "reason": "duplicate result; step already resolved"}`.
- **A result for a finished execution, or an unknown token,** is answered `{"accepted": false, "reason": "unknown token (execution finished or token invalid)"}` and not recorded.
- **Output is validated.** A success whose output does not match the task's output contract fails the attempt with `OUTPUT_CONTRACT_VIOLATION` and is answered `{"accepted": false, "reason": "output violates the task contract"}`.

Workers treat `accepted: false` as final and do not retry it. They retry result delivery on connection errors and 5xx responses until the runtime answers; the official SDKs do this.

## Crash recovery

The runtime can be killed at any moment, including with `SIGKILL` or a power loss. On the next start it:

1. loads the workflow definitions;
2. loads every execution that is not terminal and replays its history;
3. re-indexes the tokens of in-flight attempts, so results for them are accepted;
4. runs the decider, which fires overdue timers and retries, applies timeouts that passed during the outage and schedules ready steps;
5. waits one session timeout for workers to reconnect and report in-flight results; the SDKs keep retrying result delivery during the outage. After that window, attempts nobody reported are redelivered.

Workers whose session was lost get `404 UNKNOWN_SESSION` on their next call, register again and continue. Tokens remain valid across sessions and restarts.

Child workflows are started with a deterministic id (`<parent>.<unit>`) and reconciled with their parents on every tick, so a crash between starting a child and recording it neither duplicates nor loses the child.

The end-to-end test `test_z_crash_recovery` kills the runtime with `SIGKILL` while a payment is being charged, restarts it, and checks that the execution completes with exactly one recorded success for the charge.

## Immutable definitions and versioning

- A definition is identified by `(name, version)` and carries a SHA-256 `definition_hash` of its semantic content. Source locations are excluded, so reformatting a file does not change the hash.
- Registering identical content is a no-op. Registering different content under an existing name and version is refused with `409 DEFINITION_CONFLICT`; bump the version instead.
- Every execution is pinned to the hash it started with. Deploying a new version never changes running executions, and the old version stays registered for them.
- A parent definition pins the exact hash of each child workflow it starts; the child must be registered first (`CHILD_NOT_REGISTERED` otherwise). `orbit deploy` registers children before parents.
- Starting an execution with a client-chosen `execution_id` is idempotent: a repeated start returns the existing execution (`created: false`) instead of creating a duplicate. The same id with a different definition is refused with `409 EXECUTION_EXISTS`.

See [Versioning](../language/versioning.md).

## Deterministic decisions

The decider is a pure function of the history, the definition and the current time. Expressions cannot read the clock or perform I/O. Inputs and idempotency keys are evaluated once and stored in the history, and `for` lists are evaluated once and stored in `StepFanOut`, so retries, redeliveries and recovery always use the same values.

## What is not guaranteed

- **Exactly-once side effects.** Use idempotency keys.
- **Stopping external work.** Cancellation and timeouts are cooperative. The worker is told through the heartbeat `cancel` list, but the runtime cannot stop an operation that is already running in another system.
- **Ordering between independent steps.** Steps without a dependency between them may run in any order or concurrently. Among queued tasks, a worker receives the oldest matching one, but this is not a fairness guarantee across executions.
- **Timing precision.** Timers, retries and timeouts are checked every 100 ms and fire after their due time, not exactly at it. During an outage they fire when the runtime is back.
- **High availability.** A single server is a single node: while it is down, no steps are scheduled and no results are accepted; workers retry. With PostgreSQL, run several servers with `--cluster` so a standby takes over within seconds; one leader still does all the work. With `--cluster-mode shared` every server works, and the others take over a failed server's executions. See [Running several servers](cluster.md).
- **Exactly-once compensation.** Compensations are tasks with the same at-least-once delivery, and need idempotent workers too.
- **Telemetry delivery.** Spans are exported on a best-effort basis; they are dropped if the collector is unreachable or the queue is full. Metrics are in-memory counters that reset when the runtime restarts.
- **Capability-based permissions beyond task patterns, and per-execution resource limits.** These are not implemented.

## Shared mode

With `--cluster --cluster-mode shared`, several servers execute workflows at once. See [Shared mode](cluster.md#shared-mode-several-active-servers) for how it works.

What stays the same:

- **One writer per execution.** Each running execution has one owner. Only the owner appends to its history, and every append checks the owner's epoch in the same transaction, so a server that lost an execution to another one cannot write to it (`STORAGE_ERROR` starting with `fenced:`).
- **Events before effects.** History, projection, queue rows and consumed inbox entries are written in one transaction.
- **At-least-once delivery with stable idempotency keys**, uncertain attempts redelivered and never assumed failed, late results accepted for unresolved steps.
- **Commands are applied once and in order.** A signal, cancellation, result or dispatch that reaches a server that does not own the execution goes to the owner's inbox, and the owner deletes the entry in the same transaction as the events it causes. A server that dies after applying an entry has also deleted it.
- **Deterministic decisions, immutable definitions, idempotent starts.**

What changes:

| Topic | One server | Shared mode |
|---|---|---|
| Task handed out | After `StepDispatched` is committed. | After the claim of the queue row and its inbox entry are committed. The owner records `StepDispatched` a moment later; a result that arrives first waits behind it in the inbox. |
| Result acknowledged | After `StepSucceeded` or `StepAttemptFailed` is committed; `accepted` reflects the outcome. | The same on the owner. On another server: once the result is stored in the owner's inbox, with `{"accepted": true}`; the owner records a contract violation or a duplicate in history as usual. |
| Signal `seq` | Always set. | `null` when the owner has not recorded the signal within 10 seconds; the signal is still recorded once. |
| Extra redelivery cause | | A server that handed out a task died, and the result did not arrive within `--node-timeout` plus `--session-timeout`: the worker may never have received the task. The original attempt's late result is still accepted. |
| Lost worker detected | One `--session-timeout` after its last contact. | About one second later: contact is written to the database every 250 ms and read by owners every 500 ms. |
| Timers, retries, timeouts | Fired by the server's clock. | Fired by the owner's clock, after a takeover by the new owner's. Keep server clocks synchronized (NTP). |
| `--max-active-executions` | Running executions. | Running executions this server owns. Each server applies its own limit. |
| Metrics | The server's. | Per server. Sum them across servers. |

## See also

- [Executions and history](executions.md)
- [Idempotency](../language/idempotency.md)
- [Compensation](../language/compensation.md)
- [Worker protocol](../reference/protocol.md)
- [Storage](storage.md)
