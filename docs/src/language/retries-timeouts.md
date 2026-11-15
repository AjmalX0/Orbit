# Retries and timeouts

Retries and timeouts bound how long a step may take and how often the runtime
tries it again. A `retry` block sets the policy for one step, `timeout` and
`queue_timeout` limit each attempt, and the workflow `timeout` limits the whole
execution. This page lists every setting with its default, and describes
exactly when the runtime retries.

## Retry policy

```orbit
step charge {
    worker "payments.charge"
    with { order_id: input.order_id, amount: input.amount, currency: input.currency }
    retry {
        attempts: 5
        backoff: exponential
        initial_delay: 500ms
        max_delay: 10s
        non_retryable: ["CARD_DECLINED", "INSUFFICIENT_FUNDS"]
    }
}
```

`retry` is followed directly by `{` (no `:`). Inside, each setting is
`name: value`, separated by commas, semicolons or newlines.

| Setting | Type | Default | Meaning |
|---------|------|---------|---------|
| `attempts` | integer literal, 1 to 1000 | `1` | Total number of attempts, including the first. `1` means no retries. |
| `backoff` | `fixed` or `exponential` | `fixed` | How the delay between attempts grows. |
| `initial_delay` | duration literal | `1s` | The delay before the first retry. |
| `max_delay` | duration literal | `60s`, or `initial_delay` if that is longer | The upper bound for exponential delays. Must be at least `initial_delay`. |
| `non_retryable` | list of string literals | `[]` | Error codes that fail the step immediately, without retrying. |

A step without a `retry` block makes one attempt. A `retry` block that omits a
setting uses that setting's default, so `retry { attempts: 3 }` retries twice
with a fixed 1 second delay.

Every value must be a literal: expressions such as `attempts: input.n` are
rejected. The compiler reports every problem with a `retry` block as `E0410`:

| Problem | Message |
|---------|---------|
| `attempts` outside 1 to 1000 | `` `attempts` must be between 1 and 1000 `` |
| non-literal `attempts` | `` `attempts` must be an integer literal `` |
| unknown backoff | `` unknown backoff strategy `linear` `` |
| non-duration delay | `` `initial_delay` must be a duration literal such as `5s` `` |
| `max_delay` below `initial_delay` | `` `max_delay` must be at least `initial_delay` `` |
| non-string error code | `error codes must be string literals` |
| unknown setting | `` unknown retry setting `backof` `` (with a suggestion) |
| repeated setting | `` duplicate retry setting `attempts` `` |

## Backoff

The delay before attempt *n* (attempt 2 is the first retry) is:

| Backoff | Delay before attempt *n* |
|---------|--------------------------|
| `fixed` | `initial_delay` |
| `exponential` | `min(initial_delay × 2^(n−2), max_delay)` |

For `attempts: 6, backoff: exponential, initial_delay: 500ms, max_delay: 4s`:

| Attempt | 1 | 2 | 3 | 4 | 5 | 6 |
|---------|---|---|---|---|---|---|
| Delay before it | none | 500 ms | 1 s | 2 s | 4 s | 4 s |

The delay is a durable timer: a retry that is waiting survives a runtime
restart, and the attempt starts when the timer fires.

## When the runtime retries

After an attempt fails, the runtime schedules another attempt only if all of
these hold:

1. the error is retryable,
2. the error code is not listed in `non_retryable` (codes are compared
   exactly, including case),
3. attempts remain (`attempt < attempts`),
4. the workflow is not being aborted.

Otherwise the step fails permanently. If the policy allowed more than one
attempt and they were all used, the error message ends with
`(gave up after N attempts)`.

Whether an error is retryable is decided by whoever reports it. Workers report
errors with a code and a retryable flag (see
[How workers work](../sdks/overview.md)). Errors that the runtime generates
have a fixed classification:

| Code | Retryable | Cause |
|------|-----------|-------|
| `TIMEOUT` | yes | The attempt exceeded the step `timeout`. |
| `HTTP_ERROR` | depends | Network failures and `5xx`/`429` responses are retryable; invalid requests are not. See [HTTP adapter](http-adapter.md). |
| `QUEUE_TIMEOUT` | no | No worker picked the task up within `queue_timeout`. |
| `INPUT_CONTRACT_VIOLATION` | no | The evaluated input does not match the contract. |
| `OUTPUT_CONTRACT_VIOLATION` | no | The worker's output does not match the contract. |
| `EXPRESSION_ERROR` | no | Evaluating `with`, `when`, `for` or `idempotency_key` failed. |
| `ADAPTER_DISABLED` | no | A built-in task ran on a server without the adapter enabled. |

Every retry reuses the input and idempotency key that were evaluated when the
step was first scheduled. Redelivery after a lost worker is not a retry and
does not use up an attempt; see [Delivery guarantees](../runtime/guarantees.md).

## Step timeout

`timeout` limits a single attempt, measured from the moment the task is
dispatched to a worker until its result arrives:

```orbit
step analyze {
    worker "risk.analyze"
    with { customer_id: input.customer_id, amount: input.amount }
    timeout 10s
    retry { attempts: 3, backoff: exponential, initial_delay: 1s }
}
```

| Property | Value |
|----------|-------|
| Default | no limit (`orbit.http.request` uses 30 seconds when no timeout is set) |
| Applies to | task steps, workflow steps and `compensate` blocks |
| On expiry | the attempt fails with `TIMEOUT`, which is retryable |
| Zero | rejected (`E0410`) |

Timeouts are cooperative. The worker is told to stop, through the cancellation
list it receives with heartbeat responses, but the runtime cannot force an
external operation to stop. If the timed-out attempt later reports success
while the step is still unresolved, the runtime accepts that result, because
the work did happen.

On a `workflow` step, `timeout` limits how long the parent waits for the
child. When it expires, the step fails with `TIMEOUT` (workflow steps cannot
retry) and the child execution is cancelled.

## Queue timeout

`queue_timeout` limits how long an attempt may wait for a worker to pick it up:

```orbit
step charge {
    worker "payments.charge"
    with { order_id: input.order_id, amount: input.amount, currency: input.currency }
    queue_timeout 2m
}
```

| Property | Value |
|----------|-------|
| Default | no limit: a task waits until a matching worker connects |
| Measured from | the moment the attempt is scheduled (each retry starts a new wait) |
| Applies to | task steps and `compensate` blocks; not workflow or sleep steps |
| On expiry | the step fails with `QUEUE_TIMEOUT`, which is never retried |
| Redeliveries | not limited by `queue_timeout` |
| Zero | rejected (`E0410`) |

Use `queue_timeout` to fail fast when no worker for a task (or for the
`using` runtime) is running, instead of waiting until the workflow deadline.

## Workflow timeout

The workflow-level `timeout` limits the whole execution, measured from its
creation, across all steps, retries and sleeps. When it passes, the runtime
stops scheduling work, waits for in-flight attempts, runs compensations, and
ends the execution as `TIMED_OUT`. See [Workflows](workflows.md#timeout).

## Where each setting is allowed

| Setting | Task step | Workflow step | Sleep step | `compensate` block |
|---------|:---------:|:-------------:|:----------:|:------------------:|
| `retry` | yes | no (`E0307`) | no (`E0307`) | yes |
| `timeout` | yes | yes | no (`E0307`) | yes |
| `queue_timeout` | yes | no (`E0307`) | no (`E0307`) | yes |

A child workflow is durable on its own. Put retries on the child's steps.

## Retries in the history

Each attempt appears in the execution history. A step with
`retry { attempts: 3, backoff: exponential, initial_delay: 100ms }` that fails
every time produces events like these (`orbit logs <id>`):

```text
+   106ms   #9  StepScheduled             step=broken attempt=1
+   201ms  #10  StepDispatched            step=broken attempt=1 worker_id=orbit-runtime
+   203ms  #11  StepAttemptFailed         step=broken attempt=1 error=HTTP_ERROR request failed: io: Connection refused (os error 111)
+   205ms  #12  RetryScheduled            step=broken delay_ms=100
+   401ms  #13  StepScheduled             step=broken attempt=2
+   403ms  #14  StepDispatched            step=broken attempt=2 worker_id=orbit-runtime
+   406ms  #15  StepAttemptFailed         step=broken attempt=2 error=HTTP_ERROR request failed: io: Connection refused (os error 111)
+   407ms  #16  RetryScheduled            step=broken delay_ms=200
+   701ms  #17  StepScheduled             step=broken attempt=3
+   702ms  #18  StepDispatched            step=broken attempt=3 worker_id=orbit-runtime
+   704ms  #19  StepAttemptFailed         step=broken attempt=3 error=HTTP_ERROR request failed: io: Connection refused (os error 111)
+   705ms  #20  StepFailed                step=broken error=HTTP_ERROR request failed: io: Connection refused (os error 111) (gave up after 3 attempts)
```

## See also

- [Steps](steps.md)
- [Idempotency](idempotency.md)
- [Delivery guarantees](../runtime/guarantees.md)
- [Executions and history](../runtime/executions.md)
- [How workers work](../sdks/overview.md)
