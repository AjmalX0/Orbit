# Idempotency

ORBIT delivers every task at least once. A worker can receive the same task
more than once, for example after a network failure hid its result or after a
runtime restart, so workers must make their side effects idempotent. Every
task carries an idempotency key for that purpose. This page explains the
default key, how to set your own with `idempotency_key`, and how keys behave
across retries, fan-out and compensation.

## The default key

If a step does not set `idempotency_key`, its key is:

```text
<execution id>/<unit key>
```

| Unit | Example key |
|------|-------------|
| A step | `7f3c…/process_payment` |
| Element 2 of a `for` step | `7f3c…/reserve[2]` |
| A step's compensation | `7f3c…/compensate:process_payment` |
| The compensation of element 2 | `7f3c…/compensate:reserve[2]` |

The default key is unique per execution and unit, and stable across retries
and redeliveries.

## Setting a key

`idempotency_key` takes an expression of type `String`:

```orbit
step process_payment {
    worker "payments.charge"
    with { order_id: input.order_id, amount: input.amount, currency: input.currency }
    idempotency_key: "charge-" + input.order_id
}
```

Set your own key when the deduplication should span executions. In the
example above, starting the workflow twice for the same order still charges
the card once, provided the payment worker (or the payment provider) honors
the key.

Rules:

- The expression must be a `String`. Another type is `E0400`, and a `String?`
  is `E0405`. Convert numbers with [`string`](functions.md#string).
- The expression can read `input`, `execution`, other steps (which become
  dependencies) and, in a `for` step or its compensation, the loop variable.
- It is evaluated once, when the unit is first scheduled, and stored in the
  history. Retries and redeliveries reuse it unchanged.
- If it evaluates to `null` at runtime (possible only through a `JSON`
  value), the default key is used.
- An evaluation error fails the step with `EXPRESSION_ERROR`.
- `idempotency_key` is allowed on task steps and in `compensate` blocks. It is
  not allowed on workflow steps or sleep steps (`E0307`).

## Keys in fan-out steps

In a `for` step the key expression is evaluated separately for each element,
with the loop variable bound. Include something that identifies the element,
otherwise every element gets the same key and a worker that deduplicates by
key would run only one of them:

```orbit
step reserve {
    for item in input.items
    worker "inventory.reserve"
    with { order_id: input.order_id, items: [item] }
    idempotency_key: input.order_id + "/" + item.sku
}
```

If you omit `idempotency_key`, the default key already includes the element
index (`<execution id>/reserve[0]`, `<execution id>/reserve[1]`, ...).

## Keys in compensations

A compensation has its own key, `<execution id>/compensate:<unit>` by
default. You can set a business key in the `compensate` block, and it may read
the compensated step's output:

```orbit
step process_payment {
    worker "payments.charge"
    with { order_id: input.order_id, amount: input.amount, currency: input.currency }
    idempotency_key: "charge-" + input.order_id
    compensate {
        worker "payments.refund"
        with { transaction_id: process_payment.transaction_id, amount: input.amount }
        idempotency_key: "refund-" + process_payment.transaction_id
    }
}
```

## Child workflows

Workflow steps do not take an idempotency key. The child execution has a
deterministic id, `<parent id>.<step>` (or `<parent id>.<step>.<i>` for an
element of a `for` step), so the runtime never starts the same child twice.
See [Child workflows](child-workflows.md).

## Starting executions idempotently

The same idea applies to starting executions. If you pass a client-chosen
execution id (`orbit run --id`, or `execution_id` in the start API), starting
twice with the same id returns the existing execution instead of creating a
new one. Combined with the default keys, which contain the execution id, this
makes a retried start safe end to end. See
[Executions and history](../runtime/executions.md).

## What workers should do with the key

A worker receives the key with every task. Typical uses:

- pass it to an external API that supports idempotency keys,
- record it with the side effect in the same database transaction, and return
  the recorded result when the key is seen again,
- use it as a natural primary key for the record the task creates.

ORBIT does not promise exactly-once execution of external side effects. It
promises that the key is stable, and that uncertain outcomes are redelivered
with the same key rather than assumed to have failed. See
[Delivery guarantees](../runtime/guarantees.md).

## See also

- [Delivery guarantees](../runtime/guarantees.md)
- [Retries and timeouts](retries-timeouts.md)
- [Fan-out](fan-out.md)
- [Compensation](compensation.md)
- [How workers work](../sdks/overview.md)
