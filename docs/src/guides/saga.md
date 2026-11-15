# Orders with compensation

This guide builds an order-processing workflow that validates an order, scores
its risk, reserves inventory, charges the customer and sends a confirmation,
using Python and TypeScript workers. You run it end to end, then exercise
every path: a held order, a declined card that triggers compensation, a
cancellation in the middle of a payment, a transient failure that is retried,
and a duplicate charge that idempotency prevents.

The workflow is the Saga pattern: each step that changes the world declares
the business action that undoes it, and the runtime runs those actions in
reverse order when a later step fails.

## Before you begin

The guide uses files from the ORBIT repository. Clone it and run every command
from the repository root:

```bash
git clone https://github.com/AjmalX0/Orbit.git
cd Orbit
```

You need:

- `orbit` 0.1.0 or later on your `PATH` ([Installation](../getting-started/installation.md)).
- Python 3.9 or later and Node.js 22.18 or later.

| File | Contents |
|---|---|
| `examples/contracts.orbit` | Schemas and task contracts shared by the order examples |
| `examples/order_processing.orbit` | The `OrderProcessing` workflow |
| `examples/workers/python/order_workers.py` | Python worker `orders-python`: `risk.analyze`, `payments.charge`, `payments.refund` (and the shipping tasks used in [Fan-out and child workflows](fulfilment.md)) |
| `examples/workers/typescript/order_workers.ts` | TypeScript worker `orders-typescript`: `orders.validate`, `inventory.reserve`, `inventory.release`, `notifications.send` |

The example workers load the SDKs from the repository (`sdks/python` and
`sdks/typescript/src`) so they run without installing packages. In your own
project, install `orbit-lang-sdk` and `orbit-lang` instead and import them by
package name.

## The contracts

`examples/contracts.orbit` declares one task per worker capability. Two of
them are compensations: `inventory.release` undoes `inventory.reserve`, and
`payments.refund` undoes `payments.charge`.

```orbit
schema LineItem {
    sku: String
    quantity: Int
}

task "inventory.reserve" {
    version "1.0.0"
    input { order_id: String, items: List<LineItem> }
    output { reservation_id: String }
}

task "inventory.release" {
    version "1.0.0"
    input { reservation_id: String }
    output { released: Bool }
}

schema PaymentRequest {
    order_id: String
    amount: Float
    currency: String
}

schema PaymentResult {
    transaction_id: String
    status: String
}

task "payments.charge" {
    version "1.0.0"
    input PaymentRequest
    output PaymentResult
}

task "payments.refund" {
    version "1.0.0"
    input { transaction_id: String, amount: Float }
    output { refunded: Bool }
}
```

The file also declares `orders.validate`, `risk.analyze` and
`notifications.send`.

## The workflow

`examples/order_processing.orbit` imports the contracts and declares five
steps:

```orbit
import "contracts.orbit"

workflow OrderProcessing {
    version "1.0.0"
    description "Validate, risk-score, reserve, charge and notify for one order"
    timeout 5m

    input {
        order_id: String
        customer_id: String
        amount: Float
        currency: String = "USD"
        items: List<LineItem>
    }

    output {
        status: String
        transaction_id: String?
        risk_score: Float
    }

    step validate_order {
        worker "orders.validate"
        using typescript
        with { order_id: input.order_id, amount: input.amount, items: input.items }
        timeout 10s
    }

    step analyze_risk {
        worker "risk.analyze"
        using python
        with { customer_id: input.customer_id, amount: input.amount }
        retry {
            attempts: 3
            backoff: exponential
            initial_delay: 1s
            max_delay: 30s
        }
    }

    step reserve_inventory {
        worker "inventory.reserve"
        using typescript
        after validate_order
        with { order_id: input.order_id, items: input.items }
        compensate {
            worker "inventory.release"
            with { reservation_id: reserve_inventory.reservation_id }
        }
    }

    step process_payment {
        worker "payments.charge"
        using python
        after reserve_inventory
        when analyze_risk.score < 0.8
        with {
            order_id: input.order_id
            amount: input.amount
            currency: input.currency
        }
        idempotency_key: "charge-" + input.order_id
        timeout 30s
        retry { attempts: 5, backoff: exponential, initial_delay: 500ms, max_delay: 10s }
        compensate {
            worker "payments.refund"
            with { transaction_id: process_payment.transaction_id, amount: input.amount }
            idempotency_key: "refund-" + input.order_id
        }
    }

    step send_notification {
        worker "notifications.send"
        using typescript
        after process_payment
        with {
            customer_id: input.customer_id
            message: if process_payment.transaction_id == null
                then "Order " + input.order_id + " is held for review"
                else "Order " + input.order_id + " confirmed"
        }
    }

    return {
        status: if process_payment.transaction_id == null then "held_for_review" else "completed"
        transaction_id: process_payment.transaction_id
        risk_score: analyze_risk.score
    }
}
```

The step graph that the compiler derives:

```text
validate_order (ts)          analyze_risk (py)
       │                            │
       ▼                            │ when analyze_risk.score < 0.8
reserve_inventory (ts) ──┐          │
  compensate: inventory.release     │
                         ▼          ▼
                     process_payment (py)
                       compensate: payments.refund
                              │
                              ▼
                     send_notification (ts)
```

`orbit graph examples/order_processing.orbit` prints the same graph as a
Mermaid flowchart. The important decisions:

- **Parallelism comes from the data.** `validate_order` and `analyze_risk`
  share no dependency, so they run at the same time.
- **`after` orders steps that share no data.** `reserve_inventory` does not
  read `validate_order`'s output, but must not run before validation
  succeeds.
- **`when` creates an implicit dependency.** `process_payment` reads
  `analyze_risk.score`, so it waits for `analyze_risk`. If the score is 0.8 or
  more, the payment is skipped.
- **A skipped step's output is `null`.** Because `process_payment` is
  conditional, `process_payment.transaction_id` has type `String?`. The
  workflow handles it explicitly in `send_notification` and in `return`, and
  the output declares `transaction_id: String?`. The compiler rejects code
  that ignores the `null` case; see [E0405](#the-compiler-catches-missing-null-handling).
- **Each side effect has a compensation.** If anything fails after
  `reserve_inventory` succeeded, the runtime releases the reservation. If
  anything fails after `process_payment` succeeded, it refunds the charge.
- **The charge has a business idempotency key.** `"charge-" + input.order_id`
  is the same for every execution of the same order, so the payment worker can
  refuse to charge twice even if the order is submitted twice.

Check it:

```bash
orbit check examples/order_processing.orbit
```

```text
ok OrderProcessing@1.0.0 (5 steps)
```

## Run it

Start the runtime in one terminal:

```bash
orbit server
```

Start the workers in two more terminals:

```bash
PYTHONPATH=sdks/python python3 examples/workers/python/order_workers.py
```

```bash
node examples/workers/typescript/order_workers.ts
```

Run an order:

```bash
orbit run examples/order_processing.orbit --input '{
  "order_id": "A-1", "customer_id": "c-42", "amount": 120.0,
  "items": [{"sku": "BOOK-1", "quantity": 2}]
}'
```

```text
deployed OrderProcessing@1.0.0 (sha256:4fdfac082bf3)
started execution ff2cbf0c-ccb6-42e1-98ac-84ec85a2196b
  ✓ analyze_risk                   succeeded 7ms
  ✓ validate_order                 succeeded 13ms
  ✓ reserve_inventory              succeeded 11ms
  ✓ process_payment                succeeded 8ms
  ✓ send_notification              succeeded 8ms

COMPLETED ff2cbf0c-ccb6-42e1-98ac-84ec85a2196b  OrderProcessing@1.0.0
  elapsed: 56ms
{
  "status": "completed",
  "transaction_id": "tx-A-1",
  "risk_score": 0.101
}
```

The example workers have built-in behavior so that you can trigger each path
from the input:

| Input | Behavior |
|---|---|
| `customer_id` starting with `risky` | `risk.analyze` scores 0.95, so the payment is skipped |
| `amount` above 5000 | `payments.charge` fails with `CARD_DECLINED` (not retryable) |
| `amount` exactly 4242 | `payments.charge` takes 3 seconds |
| `ORBIT_FLAKY=1` set for the Python worker | the first `risk.analyze` attempt of each execution fails with a retryable error |

## Held for review: a skipped step

```bash
orbit run examples/order_processing.orbit --input '{
  "order_id": "A-3", "customer_id": "risky-7", "amount": 120.0,
  "items": [{"sku": "BOOK-1", "quantity": 1}]
}'
```

```text
up to date OrderProcessing@1.0.0 (sha256:4fdfac082bf3)
started execution 1e10703f-37cc-42ac-ae1e-c4e4ce1eeb5a
  ✓ analyze_risk                   succeeded 18ms
  ✓ validate_order                 succeeded 16ms
  ✓ reserve_inventory              succeeded 6ms
  ↷ process_payment                skipped
  ✓ send_notification              succeeded 7ms

COMPLETED 1e10703f-37cc-42ac-ae1e-c4e4ce1eeb5a  OrderProcessing@1.0.0
  elapsed: 69ms
{
  "status": "held_for_review",
  "transaction_id": null,
  "risk_score": 0.95
}
```

A skipped step is not a failure. `send_notification` depends on
`process_payment` and still runs, with the "held for review" message.

## Card declined: compensation

```bash
orbit run examples/order_processing.orbit --input '{
  "order_id": "A-2", "customer_id": "c-42", "amount": 9000,
  "items": [{"sku": "BOOK-1", "quantity": 2}]
}'
```

```text
up to date OrderProcessing@1.0.0 (sha256:4fdfac082bf3)
started execution 3647177c-74f4-4569-968f-824e267dce4a
  ✓ analyze_risk                   succeeded 4ms
  ✓ validate_order                 succeeded 4ms
  ✓ reserve_inventory              succeeded 4ms
  ✗ process_payment                failed charge of 9000 USD declined
  ⊘ send_notification              cancelled
  ✓ compensate:reserve_inventory   succeeded 4ms

FAILED 3647177c-74f4-4569-968f-824e267dce4a  OrderProcessing@1.0.0
  elapsed: 26ms
  compensation: completed
  error: CARD_DECLINED step `process_payment` failed: charge of 9000 USD declined
```

`orbit run` exits with status 1. The history shows each decision:

```bash
orbit logs 3647177c-74f4-4569-968f-824e267dce4a
```

```text
+     0ms   #1  WorkflowCreated
+     0ms   #2  WorkflowStarted
+     1ms   #3  StepScheduled             step=validate_order attempt=1
+     1ms   #4  StepScheduled             step=analyze_risk attempt=1
+     3ms   #5  StepDispatched            step=analyze_risk attempt=1 worker_id=orders-python
+     5ms   #6  StepDispatched            step=validate_order attempt=1 worker_id=orders-typescript
+     7ms   #7  StepSucceeded             step=analyze_risk attempt=1
+     9ms   #8  StepSucceeded             step=validate_order attempt=1
+     9ms   #9  StepScheduled             step=reserve_inventory attempt=1
+    10ms  #10  StepDispatched            step=reserve_inventory attempt=1 worker_id=orders-typescript
+    14ms  #11  StepSucceeded             step=reserve_inventory attempt=1
+    15ms  #12  StepScheduled             step=process_payment attempt=1
+    15ms  #13  StepDispatched            step=process_payment attempt=1 worker_id=orders-python
+    19ms  #14  StepAttemptFailed         step=process_payment attempt=1 error=CARD_DECLINED charge of 9000 USD declined
+    20ms  #15  StepFailed                step=process_payment error=CARD_DECLINED charge of 9000 USD declined
+    20ms  #16  StepCancelled             step=send_notification
+    20ms  #17  CompensationStarted       reason=step `process_payment` failed: charge of 9000 USD declined
+    20ms  #18  StepScheduled             step=compensate:reserve_inventory attempt=1
+    22ms  #19  StepDispatched            step=compensate:reserve_inventory attempt=1 worker_id=orders-typescript
+    26ms  #20  StepSucceeded             step=compensate:reserve_inventory attempt=1
+    26ms  #21  CompensationCompleted
+    26ms  #22  WorkflowFailed            error=CARD_DECLINED step `process_payment` failed: charge of 9000 USD declined
```

What happened:

1. The Python worker raised
   `TaskError("CARD_DECLINED", ..., retryable=False)`. Although
   `process_payment` allows five attempts, a non-retryable error fails the step
   at once (#14, #15).
2. `send_notification` had not started, so the runtime withdrew it (#16).
3. The runtime compensated every succeeded step that has a `compensate` block.
   Only `reserve_inventory` qualifies: `process_payment` never succeeded, so
   there is nothing to refund (#17 to #21).
4. The execution ended `FAILED` with `compensation: completed` (#22).

To make an error code permanent from the workflow side instead of the worker,
list it in the step's retry policy:
`retry { attempts: 5, non_retryable: ["CARD_DECLINED"] }`.

## Cancellation during a payment

Cancellation also triggers compensation. Use the 4242 amount, which makes the
charge take three seconds, start the execution with a known id, and cancel it
while the charge is in flight:

```bash
orbit run examples/order_processing.orbit --id order-A-4 --no-wait --input '{
  "order_id": "A-4", "customer_id": "c-42", "amount": 4242,
  "items": [{"sku": "BOOK-1", "quantity": 1}]
}'
orbit cancel order-A-4
```

```text
up to date OrderProcessing@1.0.0 (sha256:4fdfac082bf3)
started execution order-A-4
order-A-4
cancellation requested; execution is RUNNING
```

A few seconds later:

```bash
orbit status order-A-4
```

```text
CANCELLED order-A-4  OrderProcessing@1.0.0
  elapsed: 3.0s
  compensation: completed

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  validate_order               SUCCEEDED          1      8ms  worker orders-typescript
  analyze_risk                 SUCCEEDED          1      9ms  worker orders-python
  reserve_inventory            SUCCEEDED          1      7ms  worker orders-typescript
  send_notification            CANCELLED          0
  process_payment              SUCCEEDED          1     3.0s  worker orders-python
  compensate:process_payment   SUCCEEDED          1      4ms  worker orders-python
  compensate:reserve_inventory SUCCEEDED          1      5ms  worker orders-typescript
```

```bash
orbit logs order-A-4
```

```text
...
+    22ms  #12  StepScheduled             step=process_payment attempt=1
+    23ms  #13  StepDispatched            step=process_payment attempt=1 worker_id=orders-python
+    1.0s  #14  CancelRequested           reason=requested via API
+    1.0s  #15  StepCancelled             step=send_notification
+    3.0s  #16  StepSucceeded             step=process_payment attempt=1
+    3.0s  #17  CompensationStarted       reason=cancelled: requested via API
+    3.0s  #18  StepScheduled             step=compensate:process_payment attempt=1
+    3.0s  #19  StepDispatched            step=compensate:process_payment attempt=1 worker_id=orders-python
+    3.0s  #20  StepSucceeded             step=compensate:process_payment attempt=1
+    3.0s  #21  StepScheduled             step=compensate:reserve_inventory attempt=1
+    3.0s  #22  StepDispatched            step=compensate:reserve_inventory attempt=1 worker_id=orders-typescript
+    3.0s  #23  StepSucceeded             step=compensate:reserve_inventory attempt=1
+    3.0s  #24  CompensationCompleted
+    3.0s  #25  WorkflowCancelled
```

This run shows two rules of compensation:

- **The runtime waits for in-flight work.** The charge was already running
  when the cancellation arrived (#14). The runtime cannot know whether an
  external call has taken effect, so it waited for the result. The charge
  succeeded (#16), so it needed refunding.
- **Compensations run one at a time, newest first.** The refund (#18 to #20)
  ran before the inventory release (#21 to #23), the reverse of the order in
  which the steps completed.

The refund carries the idempotency key declared in its `compensate` block,
`refund-A-4`, which `orbit inspect order-A-4` shows on the
`compensate:process_payment` unit. Without one, a compensation's key defaults
to `<execution>/compensate:<unit>`.

## Transient failures: retries

Restart the Python worker with `ORBIT_FLAKY=1`, so that the first risk-scoring
attempt of each execution fails with a retryable error:

```bash
ORBIT_FLAKY=1 PYTHONPATH=sdks/python python3 examples/workers/python/order_workers.py
```

```bash
orbit run examples/order_processing.orbit --id order-A-5 --input '{
  "order_id": "A-5", "customer_id": "c-42", "amount": 120.0,
  "items": [{"sku": "BOOK-1", "quantity": 2}]
}'
```

```text
up to date OrderProcessing@1.0.0 (sha256:4fdfac082bf3)
started execution order-A-5
  ↻ analyze_risk                   retry_wait upstream timed out
  ✓ validate_order                 succeeded 6ms
  ✓ reserve_inventory              succeeded 3ms
  ✓ analyze_risk                   succeeded 6ms
  ✓ process_payment                succeeded 10ms
  ✓ send_notification              succeeded 4ms

COMPLETED order-A-5  OrderProcessing@1.0.0
  elapsed: 1.1s
{
  "status": "completed",
  "transaction_id": "tx-A-5",
  "risk_score": 0.101
}
```

The history records the failed attempt and the backoff:

```text
+    11ms   #9  StepAttemptFailed         step=analyze_risk attempt=1 error=RISK_SERVICE_UNAVAILABLE upstream timed out
+    12ms  #10  RetryScheduled            step=analyze_risk delay_ms=1000
```

`analyze_risk` uses exponential backoff starting at `initial_delay: 1s`, so
the second attempt ran one second later and succeeded. Meanwhile
`validate_order` and `reserve_inventory`, which do not depend on the risk
score, continued. `process_payment` waited for both branches.

## Duplicate submissions: idempotency

Run the same order twice with the same execution id:

```bash
orbit run examples/order_processing.orbit --id order-A-6 --input '{
  "order_id": "A-6", "customer_id": "c-42", "amount": 120.0,
  "items": [{"sku": "BOOK-1", "quantity": 2}]
}'
```

The second time, `orbit run` prints `attached to existing execution order-A-6`
and follows the first execution instead of starting another. Starting an
execution with a client-chosen id is idempotent.

Now start the same order without `--id`. This creates a second execution, but
its payment step carries the same idempotency key, `charge-A-6`. The payment
worker remembers results by key:

```python
@worker.task("payments.charge", version="1.0.0")
def charge(input, ctx):
    if ctx.idempotency_key in _charges:
        ctx.log("duplicate charge request; returning the original result")
        return _charges[ctx.idempotency_key]
    ...
    result = {"transaction_id": f"tx-{input['order_id']}", "status": "captured"}
    _charges[ctx.idempotency_key] = result
    return result
```

The worker logs:

```text
INFO orbit duplicate charge request; returning the original result {"execution_id": "9f76c35c-...", "step": "process_payment", "attempt": 1}
```

The example keeps results in memory. A production worker stores them in its
database, or passes the key to a payment provider that deduplicates on it,
because the runtime delivers tasks at least once.

## The compiler catches missing null handling

Suppose you add a `receipts.send` contract with input
`{ transaction_id: String }` and a step that passes it the transaction id:

```orbit
step send_receipt {
    worker "receipts.send"
    with { transaction_id: process_payment.transaction_id }
}
```

`orbit check` rejects it, because `process_payment` can be skipped:

```text
error[E0405]: value may be `null` but `String` is required
  --> order_processing.orbit:97:32
   |
97 |         with { transaction_id: process_payment.transaction_id }
   |                                ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = help: provide a fallback with `coalesce(value, default)`, or declare the target as optional with `?`

error: compilation failed: 1 error(s), 0 warning(s)
```

Fix it with
`coalesce(process_payment.transaction_id, "none")`, by making the contract's
field optional (`transaction_id: String?`), or by giving the receipt step the
same `when` condition and handling the skipped case downstream.

## Design checklist

- Give every step that changes external state a `compensate` block, and make
  the compensation safe to run more than once.
- Mark business failures that retrying cannot fix as non-retryable, in the
  worker (`retryable=False`) or in the step (`non_retryable`).
- Use a business idempotency key, such as an order id, for side effects that
  must not repeat across executions, and use `--id` (or `execution_id` in the
  API) when the caller may submit the same request twice.
- Keep compensations independent of steps that may not have run. The compiler
  only lets a compensation read its own step's output and that step's
  ancestors.

## See also

- [Compensation](../language/compensation.md)
- [Conditions](../language/conditions.md)
- [Retries and timeouts](../language/retries-timeouts.md)
- [Idempotency](../language/idempotency.md)
- [Delivery guarantees](../runtime/guarantees.md)
- [Fan-out and child workflows](fulfilment.md)
