# Fan-out and child workflows

This guide builds an order-fulfilment workflow that reserves every line item
of an order in parallel, with a concurrency limit, and books shipping through
a separate `Shipping` workflow that runs as a durable child execution. You run
it end to end and then make shipping fail, to see each reservation compensated
individually.

## Before you begin

Clone the ORBIT repository and run every command from its root:

```bash
git clone https://github.com/AjmalX0/Orbit.git
cd Orbit
```

You need `orbit` 0.1.0 or later, Python 3.9+ and Node.js 22.18+. The guide
reuses the order workers from [Orders with compensation](saga.md):

| File | Contents |
|---|---|
| `examples/fulfilment.orbit` | Shipping contracts, the `Shipping` workflow and the `Fulfilment` workflow |
| `examples/contracts.orbit` | `LineItem`, `inventory.reserve`, `inventory.release` (imported) |
| `examples/workers/typescript/order_workers.ts` | `inventory.reserve`, `inventory.release` |
| `examples/workers/python/order_workers.py` | `shipping.quote`, `shipping.book` |

## The workflows

```orbit
import "contracts.orbit"

task "shipping.quote" {
    version "1.0.0"
    input { order_id: String, items: Int }
    output { carrier: String, cost: Float }
}

task "shipping.book" {
    version "1.0.0"
    input { order_id: String, carrier: String }
    output { tracking: String }
}

workflow Shipping {
    version "1.0.0"
    input { order_id: String, items: Int }
    output { carrier: String, tracking: String, cost: Float }

    step quote { worker "shipping.quote" with { order_id: input.order_id, items: input.items } }
    step book { worker "shipping.book" with { order_id: input.order_id, carrier: quote.carrier } timeout 30s }

    return { carrier: quote.carrier, tracking: book.tracking, cost: quote.cost }
}

workflow Fulfilment {
    version "1.0.0"
    input {
        order_id: String
        items: List<LineItem>
    }
    output {
        reservations: List<{ reservation_id: String }>
        tracking: String
    }

    step reserve {
        for item in input.items
        max_parallel 4
        worker "inventory.reserve"
        using typescript
        with { order_id: input.order_id + "-" + item.sku, items: [item] }
        compensate {
            worker "inventory.release"
            with { reservation_id: reserve.reservation_id }
        }
    }

    step ship {
        workflow "Shipping"
        with { order_id: input.order_id, items: len(input.items) }
        timeout 1h
    }

    return { reservations: reserve, tracking: ship.tracking }
}
```

### The fan-out step

`reserve` is a `for` step:

- `for item in input.items` runs the task once per element. `item` is in scope
  in `with` and in `compensate`, so each element reserves one SKU.
- `max_parallel 4` allows at most four elements to run at the same time.
- Each element is its own unit, `reserve[0]`, `reserve[1]`, ..., with its own
  attempts, timeouts and idempotency key (`<execution>/reserve[i]`).
- The step's output is the list of element outputs in input order. Its type is
  `List<{ reservation_id: String }>`, which is why `return` can use
  `reservations: reserve` directly.
- Inside `compensate`, `reserve.reservation_id` refers to the output of the
  element being compensated, not to the list.

An empty `items` list makes the step succeed immediately with `[]`.

### The child workflow step

`ship` is a `workflow` step. It starts `Shipping` as a separate execution:

- The compiler checks `with` against `Shipping`'s `input` and types `ship`'s
  output as `Shipping`'s `output`, so `ship.tracking` is a `String`.
- The child execution has a deterministic id, `<parent id>.ship`. The runtime
  can therefore never start it twice, even if it crashes right after starting
  it.
- `timeout 1h` limits how long the parent waits for the child. Child steps
  take no `retry`, `using`, `idempotency_key` or `queue_timeout`; retries
  belong on the child's own steps.
- `Shipping` must be defined in, or imported into, the same compilation.
  `orbit deploy` and `orbit run` register it before `Fulfilment`.

`ship` does not read `reserve`'s output and has no `after` clause, so the two
steps run concurrently. Add `after reserve` to `ship` if shipping must wait
for every reservation.

```bash
orbit check examples/fulfilment.orbit
```

```text
ok Shipping@1.0.0 (2 steps)
ok Fulfilment@1.0.0 (2 steps)
```

## Run it

Start the runtime and the two order workers, each in its own terminal:

```bash
orbit server
```

```bash
PYTHONPATH=sdks/python python3 examples/workers/python/order_workers.py
```

```bash
node examples/workers/typescript/order_workers.ts
```

Run an order with three line items:

```bash
orbit run examples/fulfilment.orbit --id ful-B-1 --input '{
  "order_id": "B-1",
  "items": [
    {"sku": "BOOK-1", "quantity": 1},
    {"sku": "PEN-2", "quantity": 3},
    {"sku": "MUG-3", "quantity": 1}
  ]
}'
```

```text
deployed Shipping@1.0.0 (sha256:e9480a2e5f6b)
deployed Fulfilment@1.0.0 (sha256:ecffc60c3bd3)
started execution ful-B-1
  ✓ reserve[0]                     succeeded 16ms
  ✓ reserve[1]                     succeeded 14ms
  ✓ reserve[2]                     succeeded 19ms
  ✓ reserve                        succeeded 27ms
  ✓ ship                           succeeded 96ms

COMPLETED ful-B-1  Fulfilment@1.0.0
  elapsed: 112ms
{
  "reservations": [
    {
      "reservation_id": "res-B-1-BOOK-1"
    },
    {
      "reservation_id": "res-B-1-PEN-2"
    },
    {
      "reservation_id": "res-B-1-MUG-3"
    }
  ],
  "tracking": "UPS-B-1"
}
```

`orbit run` deployed both workflows, callee first. `orbit status` shows one
row per element and one for the step as a whole. The `ship` unit is executed
by the runtime itself, which appears as the worker `orbit-runtime`:

```bash
orbit status ful-B-1
```

```text
COMPLETED ful-B-1  Fulfilment@1.0.0
  elapsed: 112ms

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  reserve[0]                   SUCCEEDED          1     16ms  worker orders-typescript
  reserve[1]                   SUCCEEDED          1     14ms  worker orders-typescript
  reserve[2]                   SUCCEEDED          1     19ms  worker orders-typescript
  reserve                      SUCCEEDED          1     27ms
  ship                         SUCCEEDED          1     96ms  worker orbit-runtime

  output: {"reservations":[{"reservation_id":"res-B-1-BOOK-1"},{"reservation_id":"res-B-1-PEN-2"},{"reservation_id":"res-B-1-MUG-3"}],"tracking":"UPS-B-1"}
```

The child is an execution of its own. `orbit ps` lists it next to its parent:

```bash
orbit ps
```

```text
EXECUTION                              WORKFLOW                 VERSION    STATUS        AGE
ful-B-1.ship                           Shipping                 1.0.0      COMPLETED     121ms
ful-B-1                                Fulfilment               1.0.0      COMPLETED     135ms
```

```bash
orbit status ful-B-1.ship
```

```text
COMPLETED ful-B-1.ship  Shipping@1.0.0
  elapsed: 33ms

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  quote                        SUCCEEDED          1     23ms  worker orders-python
  book                         SUCCEEDED          1      4ms  worker orders-python

  output: {"carrier":"ups","tracking":"UPS-B-1","cost":7.5}
```

## A failing child: per-element compensation

The Python worker's `shipping.book` fails with the non-retryable error
`NO_CARRIER` for order ids that start with `noship`:

```bash
orbit run examples/fulfilment.orbit --id ful-noship-1 --input '{
  "order_id": "noship-1",
  "items": [
    {"sku": "BOOK-1", "quantity": 1},
    {"sku": "PEN-2", "quantity": 3}
  ]
}'
```

```text
up to date Shipping@1.0.0 (sha256:e9480a2e5f6b)
up to date Fulfilment@1.0.0 (sha256:ecffc60c3bd3)
started execution ful-noship-1
  ✓ reserve[1]                     succeeded 5ms
  ✓ reserve[0]                     succeeded 9ms
  ✓ reserve                        succeeded 11ms
  ✗ ship                           failed child workflow `Shipping` failed: step `book` failed: no carrier serves this address
  ✓ compensate:reserve[0]          succeeded 5ms
  ✓ compensate:reserve[1]          succeeded 4ms

FAILED ful-noship-1  Fulfilment@1.0.0
  elapsed: 163ms
  compensation: completed
  error: NO_CARRIER step `ship` failed: child workflow `Shipping` failed: step `book` failed: no carrier serves this address
```

The child failed first. Its history ends with the failure of `book`:

```bash
orbit logs ful-noship-1.ship
```

```text
+     0ms   #1  WorkflowCreated
+     0ms   #2  WorkflowStarted
+     2ms   #3  StepScheduled             step=quote attempt=1
+     3ms   #4  StepDispatched            step=quote attempt=1 worker_id=orders-python
+    10ms   #5  StepSucceeded             step=quote attempt=1
+    11ms   #6  StepScheduled             step=book attempt=1
+    13ms   #7  StepDispatched            step=book attempt=1 worker_id=orders-python
+    18ms   #8  StepAttemptFailed         step=book attempt=1 error=NO_CARRIER no carrier serves this address
+    18ms   #9  StepFailed                step=book error=NO_CARRIER no carrier serves this address
+    18ms  #10  WorkflowFailed            error=NO_CARRIER step `book` failed: no carrier serves this address
```

The parent's history shows the fan-out, the child's error arriving as the
`ship` step's error, and the compensation of each reserved element:

```bash
orbit logs ful-noship-1
```

```text
+     0ms   #1  WorkflowCreated
+     0ms   #2  WorkflowStarted
+     5ms   #3  StepFanOut                step=reserve
+     5ms   #4  StepScheduled             step=ship attempt=1
+     5ms   #5  StepScheduled             step=reserve[0] attempt=1
+     5ms   #6  StepScheduled             step=reserve[1] attempt=1
+     6ms   #7  StepDispatched            step=reserve[0] attempt=1 worker_id=orders-typescript
+     9ms   #8  StepDispatched            step=reserve[1] attempt=1 worker_id=orders-typescript
+    14ms   #9  StepSucceeded             step=reserve[1] attempt=1
+    15ms  #10  StepSucceeded             step=reserve[0] attempt=1
+    16ms  #11  StepSucceeded             step=reserve attempt=1
+    51ms  #12  StepDispatched            step=ship attempt=1 worker_id=orbit-runtime
+   149ms  #13  StepAttemptFailed         step=ship attempt=1 error=NO_CARRIER child workflow `Shipping` failed: step `book` failed: no carrier serves this address
+   150ms  #14  StepFailed                step=ship error=NO_CARRIER child workflow `Shipping` failed: step `book` failed: no carrier serves this address
+   150ms  #15  CompensationStarted       reason=step `ship` failed: child workflow `Shipping` failed: step `book` failed: no carrier serves this address
+   150ms  #16  StepScheduled             step=compensate:reserve[0] attempt=1
+   151ms  #17  StepDispatched            step=compensate:reserve[0] attempt=1 worker_id=orders-typescript
+   156ms  #18  StepSucceeded             step=compensate:reserve[0] attempt=1
+   156ms  #19  StepScheduled             step=compensate:reserve[1] attempt=1
+   158ms  #20  StepDispatched            step=compensate:reserve[1] attempt=1 worker_id=orders-typescript
+   163ms  #21  StepSucceeded             step=compensate:reserve[1] attempt=1
+   163ms  #22  CompensationCompleted
+   163ms  #23  WorkflowFailed            error=NO_CARRIER step `ship` failed: child workflow `Shipping` failed: step `book` failed: no carrier serves this address
```

Points to note:

- **`StepFanOut` (#3)** records the evaluated list once. Retries and recovery
  reuse it, so the set of elements cannot change mid-execution.
- **`reserve` and `ship` were scheduled together (#4 to #6)**, because `ship`
  does not depend on `reserve`.
- **The child's error code propagates.** The `ship` step fails with the
  child's own code, `NO_CARRIER`. A child that is cancelled or times out fails
  the step with `CHILD_CANCELLED` or `CHILD_TIMED_OUT`. `orbit inspect
  ful-noship-1` records the child's id in the error details
  (`"child_execution_id": "ful-noship-1.ship"`).
- **Each element is compensated individually**, newest first.
  `reserve[0]` finished after `reserve[1]` (#10 after #9), so it was released
  first. Each compensation is its own unit with its own idempotency key, such
  as `ful-noship-1/compensate:reserve[0]`.
- **A child compensates its own steps.** `Shipping` declares no
  compensations, and its `quote` step had nothing to undo. Compensations
  declared on the child's steps run when the child itself fails, is cancelled
  or times out, including when the parent cancels it. A child that already
  completed is not rolled back when the parent fails later. To undo a
  completed child's work, give the `workflow` step its own `compensate` block
  that calls a worker task, such as one that cancels the booking.

## Variations

Limit concurrency to one element at a time to process items sequentially:

```orbit
step reserve {
    for item in input.items
    max_parallel 1
    worker "inventory.reserve"
    with { order_id: input.order_id + "-" + item.sku, items: [item] }
}
```

Start one child per element by combining `for` with `workflow`. The child ids
are `<parent>.<step>.<i>`:

```orbit
step ship_each {
    for item in input.items
    workflow "Shipping"
    with { order_id: input.order_id + "-" + item.sku, items: item.quantity }
}
```

Use `when` to decide whether the whole fan-out runs. The loop variable is not
in scope in `when`, because the condition is evaluated once for the step:

```orbit
step reserve {
    for item in input.items
    when len(input.items) <= 50
    worker "inventory.reserve"
    with { order_id: input.order_id + "-" + item.sku, items: [item] }
}
```

## See also

- [Fan-out](../language/fan-out.md)
- [Child workflows](../language/child-workflows.md)
- [Compensation](../language/compensation.md)
- [Executions and history](../runtime/executions.md)
- [Orders with compensation](saga.md)
