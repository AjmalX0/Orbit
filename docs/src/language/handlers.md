# Outcome handlers

An outcome handler is a block of steps that runs when an execution ends badly:
`on failure` when a step fails permanently, `on cancel` when the execution is
cancelled, and `on timeout` when the workflow's `timeout` passes. Handlers run
after [compensation](compensation.md) has finished and before the execution
records its terminal status, so they are the place to tell a customer, page an
operator or record an audit entry. This page covers the syntax, what handler
steps can read, the order of events, and the rules the compiler enforces.

## Syntax

The complete example is [`examples/checkout.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/checkout.orbit), with a test in `examples/checkout.test.orbit` ([Testing workflows](../tooling/testing.md)).

```orbit
import "contracts.orbit"

workflow Checkout {
    version "1.0.0"
    input { order_id: String, customer_id: String, amount: Float, items: List<LineItem> }

    step reserve {
        worker "inventory.reserve"
        with { order_id: input.order_id, items: input.items }
        compensate {
            worker "inventory.release"
            with { reservation_id: reserve.reservation_id }
        }
    }

    step charge {
        worker "payments.charge"
        after reserve
        with { order_id: input.order_id, amount: input.amount, currency: "USD" }
    }

    on failure {
        step tell_customer {
            worker "notifications.send"
            with {
                customer_id: input.customer_id
                message: "We could not complete order " + input.order_id + ": " + failure.message
            }
        }
        step summary {
            compute { code: failure.code, reserved: !is_null(reserve), charged: !is_null(charge) }
        }
    }

    on cancel {
        step confirm_cancel {
            worker "notifications.send"
            with { customer_id: input.customer_id, message: "Order " + input.order_id + " was cancelled" }
        }
    }
}
```

A workflow has at most one block of each kind. A block contains `step`
declarations and nothing else.

| Block | Runs when | `failure.reason` |
|-------|-----------|------------------|
| `on failure` | A step fails permanently, or a [resource limit](../runtime/server.md#resource-limits) is reached (`LIMIT_EXCEEDED`) | `failed` |
| `on cancel` | The execution is cancelled (`orbit cancel`, the HTTP API, or a cancelled parent) | `cancelled` |
| `on timeout` | The workflow `timeout` passes | `timed_out` |

A successful execution runs no handler. When the matching block is absent, the
execution finishes exactly as it would without handlers. A `return` expression
that fails, or a result that does not match `output`, fails the execution
directly, after every step has finished; no handler runs in that case.

## What handler steps can read

| Name | Type | Notes |
|------|------|-------|
| `failure` | `{ reason: String, code: String, message: String }` | Why the execution is ending. For a failed step, `code` is the step's error code and `message` names the step (``step `charge` failed: ...``); a history limit has code `LIMIT_EXCEEDED` and the limit's message; a cancellation has code `CANCELLED` and the cancellation reason as message; a timeout has code `WORKFLOW_TIMEOUT` and the message `workflow timed out`. |
| `input` | the workflow input | Unchanged. |
| `execution` | `{ id, workflow, version }` | As everywhere else. |
| a workflow step, e.g. `charge` | that step's type, made optional | A workflow step may not have run, or may have failed: inside handlers every workflow step reads as `null` unless it succeeded. Use `is_null` and `coalesce`. |
| an earlier step of the same block | its output type | Optional if that step has a `when`. Reading it adds a dependency, as in the main workflow. |

`failure` exists only inside handlers; using it elsewhere is error `E0401`.

## Order of events

When the payment above is declined, `orbit logs` shows compensation first, then
the handler, then the terminal event:

```text
+    59ms   #9  StepFailed                step=charge error=CARD_DECLINED charge of 9000.0 USD declined
+    59ms  #10  CompensationStarted       reason=step `charge` failed: charge of 9000.0 USD declined
+    59ms  #11  StepScheduled             step=compensate:reserve attempt=1
+    70ms  #12  StepDispatched            step=compensate:reserve attempt=1 worker_id=orders-typescript
+   150ms  #13  StepSucceeded             step=compensate:reserve attempt=1
+   151ms  #14  CompensationCompleted
+   151ms  #15  HandlersStarted           kind=failure
+   151ms  #16  StepScheduled             step=on_failure:tell_customer attempt=1
+   151ms  #17  StepSucceeded             step=on_failure:summary attempt=1
+   154ms  #18  StepDispatched            step=on_failure:tell_customer attempt=1 worker_id=orders-typescript
+   187ms  #19  StepSucceeded             step=on_failure:tell_customer attempt=1
+   187ms  #20  WorkflowFailed            error=CARD_DECLINED step `charge` failed: charge of 9000.0 USD declined
```

1. In-flight steps finish, and the steps that succeeded are compensated
   (newest first). See [Compensation](compensation.md).
2. `HandlersStarted` records the block that runs (`kind`) and the compensation
   outcome that the terminal event will carry.
3. Handler steps run in dependency order; steps without a dependency between
   them run concurrently, as in the main workflow. Their unit keys are
   `on_<kind>:<step>`, for example `on_failure:tell_customer`.
4. When every handler step has finished, the terminal event
   (`WorkflowFailed`, `WorkflowCancelled` or `WorkflowTimedOut`) is recorded,
   with the original error and compensation outcome.

## When a handler step fails

A handler step can retry like any other (`retry { ... }`). If it still fails,
the failure is recorded on its unit and the steps that depend on it are
cancelled, but **the execution's outcome does not change**: the terminal event
keeps the error that started the shutdown, not the handler's. Other handler
steps still run.

## Rules

| Rule | Diagnostic |
|------|------------|
| Only `on failure`, `on cancel` and `on timeout` exist | `E0105` |
| One block of each kind per workflow | `E0319` |
| Handler step names must differ from workflow step names and from `failure` | `E0302` |
| `after` may only name steps of the same block | `E0303` |
| Handler steps run a `worker` task, a `compute` expression or a `sleep`; `await`, child workflows, `for` and `max_parallel` are not allowed | `E0307` |
| Handler steps cannot declare `compensate` | `E0308` |
| `failure` is read only inside handler steps | `E0401` |

```text
error[E0307]: `await` is not allowed in an `on failure` step
 --> bad.orbit:6:24
  |
6 |         step x { await s }
  |                        ^
  = note: handlers run while the execution is finishing; it no longer accepts signals
```

Workflows that declare handlers compile to `orbit-ir/2`; see
[Versioning](versioning.md).

## See also

- [Compensation](compensation.md)
- [Retries and timeouts](retries-timeouts.md)
- [Executions and history](../runtime/executions.md#handler-events)
- [Running the server: resource limits](../runtime/server.md#resource-limits)
- [Compiler diagnostics](../reference/diagnostics.md)
- [Testing workflows](../tooling/testing.md)
