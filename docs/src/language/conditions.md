# Conditions

A `when` condition makes a step conditional: the step runs only if the
condition is true, and is skipped otherwise. Because a skipped step produces
no output, the type checker treats every reference to a conditional step as
optional and makes you handle the missing value. This page covers the `when`
member, skipped steps, and the patterns for working with optional results.

## The `when` member

```orbit
step manual_review {
    worker "notifications.send"
    when analyze_risk.score >= 0.8
    with { customer_id: input.customer_id, message: "Your order is under review" }
}
```

The condition is an [expression](expressions.md) of type `Bool` or `Bool?`.
Any other type is `E0408`:

```text
error[E0408]: `when` condition must be `Bool`, found `Int`
```

Like any data reference, a step read in `when` becomes a dependency (see
[Dependencies](dependencies.md)). `when` is allowed on task steps, workflow
steps and sleep steps, but not inside a `compensate` block (`E0307`).

## Evaluation

The runtime evaluates the condition once, when every dependency of the step
has finished:

| Result | Effect |
|--------|--------|
| `true` | The step is scheduled. |
| `false` | The step is skipped. |
| `null` | The step is skipped. |
| an expression error | The step fails with `EXPRESSION_ERROR`, which aborts the workflow. |

A skipped step has status `SKIPPED`, records a `StepSkipped` event, and has
the output `null`. It is never compensated, because it did nothing.

## Skipped steps and optional outputs

Because the step may be skipped, every reference to it is optional. If the
task's output is `{ transaction_id: String }`, then:

| Expression | Type |
|------------|------|
| `process_payment` | `{ transaction_id: String }?` |
| `process_payment.transaction_id` | `String?` |

Member access propagates `null`, so `process_payment.transaction_id` is
`null` when the step was skipped, rather than an error.

Steps that depend on a skipped step still run. The type checker makes sure
they handle the missing value: using an optional value where a non-optional
one is required is `E0405`.

```orbit
task "payments.charge" {
    input { order_id: String, amount: Float }
    output { transaction_id: String }
}

task "receipts.send" {
    input { order_id: String, transaction_id: String }
    output { sent: Bool }
}

workflow Pay {
    version "1.0.0"
    input { order_id: String, amount: Float }

    step charge {
        worker "payments.charge"
        when input.amount > 0.0
        with { order_id: input.order_id, amount: input.amount }
    }

    step receipt {
        worker "receipts.send"
        with { order_id: input.order_id, transaction_id: charge.transaction_id }
    }
}
```

```text
error[E0405]: value may be `null` but `String` is required
  --> pay.orbit:23:58
   |
23 |         with { order_id: input.order_id, transaction_id: charge.transaction_id }
   |                                                          ^^^^^^^^^^^^^^^^^^^^^
   = help: provide a fallback with `coalesce(value, default)`, or declare the target as optional with `?`

error: compilation failed: 1 error(s), 0 warning(s)
```

## Handling optional results

Choose one of these patterns, depending on what the downstream step should do
when the conditional step was skipped.

**Provide a fallback** with [`coalesce`](functions.md#coalesce):

```orbit
step receipt {
    worker "receipts.send"
    with { order_id: input.order_id, transaction_id: coalesce(charge.transaction_id, "none") }
}
```

**Skip the downstream step too**, with its own condition:

```orbit
step receipt {
    worker "receipts.send"
    when !is_null(charge)
    with { order_id: input.order_id, transaction_id: coalesce(charge.transaction_id, "") }
}
```

The condition does not narrow the type, so the `with` expression still needs
`coalesce`.

**Branch** with `if ... then ... else`, or compare against `null`:

```orbit
return {
    status: if charge.transaction_id == null then "not_charged" else "charged"
    transaction_id: charge.transaction_id
}
```

**Declare the target as optional.** If a contract field or a workflow output
field is `T?`, an optional value can be passed to it directly.

## Conditions and fan-out

On a `for` step, `when` decides once whether the whole step runs, before the
list is expanded. The loop variable is therefore not in scope in `when`:

```orbit
task "inventory.reserve" {
    input { sku: String }
    output { reservation_id: String }
}

workflow Reserve {
    version "1.0.0"
    input { skus: List<String> }
    step reserve {
        for sku in input.skus
        when sku != ""
        worker "inventory.reserve"
        with { sku: sku }
    }
}
```

```text
error[E0401]: loop variable `sku` is not in scope in `when`
  --> reserve.orbit:11:14
   |
11 |         when sku != ""
   |              ^^^^^^^^^
   = note: `when` decides once whether the whole `for` step runs, before it fans out
   = help: to process only some elements, filter the list in a previous step

error: compilation failed: 1 error(s), 0 warning(s)
```

A skipped `for` step has the output `null`, so its type is `List<T>?`. ORBIT
v0.1 has no per-element filter; to process only some elements, compute the
filtered list in a previous step. See [Fan-out](fan-out.md).

## Conditions and compensation

Inside a step's own `compensate` block, the step's output is not optional: a
compensation only runs for a step that succeeded. See
[Compensation](compensation.md#what-a-compensation-can-read).

## Constant conditions

A condition that reads nothing (no `input`, step output, `execution` field or
variable) has the same value in every execution. The compiler evaluates it
and warns: `W0603` when it is `false` or `null` (the step never runs) and
`W0604` when it is `true` (the `when` only makes the output optional). See
[Static safety checks](safety.md#w0603-step-never-runs).

## See also

- [Expressions](expressions.md)
- [Static safety checks](safety.md)
- [Built-in functions](functions.md)
- [Types](types.md#optional-types-and-null)
- [Dependencies](dependencies.md)
- [Fan-out](fan-out.md)
