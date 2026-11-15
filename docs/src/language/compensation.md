# Compensation

A `compensate` block declares the business action that undoes a step, such as
refunding a payment or releasing reserved stock. When a workflow fails, is
cancelled or times out, the runtime runs the compensation of every step that
succeeded, newest first. This is the Saga pattern: compensation is explicit
business logic, not a database rollback. This page covers the syntax, what a
compensation can read, when compensations run, and how their outcome is
recorded.

## Declaring a compensation

```orbit
workflow Order {
    version "1.0.0"
    input { order_id: String, amount: Float, items: List<LineItem> }

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
        compensate {
            worker "payments.refund"
            with { transaction_id: charge.transaction_id, amount: input.amount }
            idempotency_key: "refund-" + charge.transaction_id
            retry { attempts: 10, backoff: exponential, initial_delay: 1s, max_delay: 1m }
        }
    }

    step confirm {
        worker "notifications.send"
        after charge
        with { customer_id: input.order_id, message: "Order confirmed" }
    }
}
```

If `confirm` fails permanently, the runtime refunds the charge and then
releases the reservation.

## Members of a compensate block

A `compensate` block is a small task step without its own scheduling:

| Member | Allowed | Notes |
|--------|---------|-------|
| `worker "task"` | required | Missing worker: `E0308`. |
| `with` | yes | Checked against the compensation task's input. |
| `using` | yes | |
| `timeout`, `queue_timeout` | yes | |
| `retry` | yes | Defaults to one attempt, like any step. |
| `idempotency_key` | yes | Defaults to `<execution id>/compensate:<unit>`. |
| `sleep`, `workflow` | no | `E0308`: compensation must call a worker task. |
| `when`, `after`, `for`, `max_parallel` | no | `E0307` |
| `compensate` | no | Compensations cannot be nested (`E0102`). |

Steps that can have a compensation: task steps and workflow steps. A `sleep`
step has nothing to compensate (`E0308`). Declare at most one `compensate`
block per step.

## What a compensation can read

A compensation expression can read:

- `input` and `execution`,
- the step's own output, which is **not optional** here, even if the step has
  a `when` condition, because only succeeded steps are compensated,
- the outputs of the steps the step depends on, directly or transitively
  (with their usual, possibly optional, types),
- in a `for` step, the loop variable, bound to the element being compensated.

In a `for` step, a reference to the step itself is the output of the element
being compensated, not the list. See [Fan-out](fan-out.md#compensation).

Reading any other step is `E0308`, because that step may not have run when the
compensated step did:

```orbit
task "a.run" {
    input { n: Int }
    output { v: Int }
}

task "a.undo" {
    input { v: Int }
    output { ok: Bool }
}

workflow Undo {
    version "1.0.0"
    step first { worker "a.run" with { n: 1 } }
    step second {
        worker "a.run"
        with { n: 2 }
        compensate { worker "a.undo" with { v: first.v } }
    }
}
```

```text
error[E0308]: step `first` cannot be referenced here
  --> undo.orbit:17:48
   |
17 |         compensate { worker "a.undo" with { v: first.v } }
   |                                                ^^^^^
   = note: a compensation for `second` can only read `second` and the steps it depends on

error: compilation failed: 1 error(s), 0 warning(s)
```

Add `after first` to `second` to make `first` an ancestor, and the reference
becomes valid.

Compensation references do not add dependencies to the step graph.

## When compensations run

Compensation starts when the workflow is aborted, for any of these reasons:

| Reason | Final status |
|--------|--------------|
| A step (or an element of a `for` step) fails permanently | `FAILED` |
| The execution is cancelled (`orbit cancel`, the cancel API, or a parent that no longer needs a child) | `CANCELLED` |
| The workflow `timeout` passes | `TIMED_OUT` |

The runtime then proceeds in this order:

1. **Withdraw** work that has not started: pending and queued units, timers,
   `await` steps waiting for a signal, and retries waiting for their delay
   become `CANCELLED`.
2. **Wait** for attempts that are in flight. They may still succeed, and a
   step that succeeds now also needs compensating. Failed attempts are not
   retried while the workflow is aborting.
3. **Compensate** every unit that succeeded and has a `compensate` block, one
   at a time, in reverse order of completion (the step that finished last is
   compensated first). Each element of a `for` step is a separate unit.
4. **Finish** with the final status and a compensation outcome.

Steps that were skipped, failed, cancelled or never started are not
compensated. Steps without a `compensate` block are ignored.

A workflow that fails while evaluating its `return` value, after all steps
succeeded, ends `FAILED` without compensation.

## Compensation outcome

Every failed, cancelled or timed-out execution records how compensation went:

| Outcome | Meaning |
|---------|---------|
| `none` | No succeeded step had a `compensate` block. |
| `completed` | Every compensation succeeded. |
| `failed` | At least one compensation failed permanently, after its own retries. |

A compensation that fails permanently does not stop the others: the runtime
keeps undoing the remaining steps, newest first, so as few effects as possible
are left behind. When every compensation has run, it records one
`CompensationFailed` event per failed compensation and ends the execution with
`compensation: failed`. The history shows exactly which steps were undone and
which were not, so an operator can finish the cleanup. Give compensations
generous retry policies for that reason. (Version 0.1.0 stopped at the first
failed compensation.)

## Compensation in the history

For a workflow with two compensated steps, `first` and `second`, followed by a
step `broken` that fails, `orbit logs` shows:

```text
+   705ms  #20  StepFailed                step=broken error=HTTP_ERROR ...
+   705ms  #21  CompensationStarted       reason=step `broken` failed: ...
+   705ms  #22  StepScheduled             step=compensate:second attempt=1
+   806ms  #24  StepSucceeded             step=compensate:second attempt=1
+   808ms  #25  StepScheduled             step=compensate:first attempt=1
+   905ms  #27  StepSucceeded             step=compensate:first attempt=1
+   907ms  #28  CompensationCompleted
+   907ms  #29  WorkflowFailed            error=HTTP_ERROR step `broken` failed: ...
```

Compensation units are keyed `compensate:<unit>`, for example
`compensate:charge` or `compensate:reserve[2]`.

## Designing compensations

- Declare `compensated_by` on contracts whose effect must be undone. The
  compiler then warns about every step that calls the task without the
  matching compensation (`W0601`, `W0602`). See
  [Static safety checks](safety.md).

- Make compensations idempotent. They are delivered at least once, like any
  task, and their key is stable.
- Prefer business actions with clear semantics (refund, release, cancel)
  over attempts to restore previous state.
- A compensation can only read data that existed when the step completed. If
  undoing needs an identifier, return it from the step.
- Compensation is not available for side effects that cannot be undone, such
  as a sent email. Order such steps last.

## See also

- [Orders with compensation guide](../guides/saga.md)
- [Steps](steps.md)
- [Fan-out](fan-out.md#compensation)
- [Retries and timeouts](retries-timeouts.md)
- [Static safety checks](safety.md)
- [Executions and history](../runtime/executions.md)
