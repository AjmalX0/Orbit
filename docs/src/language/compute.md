# Compute steps

A `compute` step evaluates an expression in the runtime and makes the value
its output. It needs no worker, so it suits the small, deterministic pieces of
logic that sit between tasks: deriving a flag, totalling a list, reshaping a
record, picking the items that need attention. This page covers when to use a
compute step instead of a worker, the syntax and members, how the runtime
evaluates it, lambdas and collection functions, fan-out, conditions and
errors.

## When to use a compute step

| Use a compute step when | Use a worker task when |
|---|---|
| The result depends only on the workflow input and earlier step outputs. | The work calls a service, a database, a file system or the clock. |
| The logic fits in an [expression](expressions.md): arithmetic, comparisons, conditionals, the [built-in functions](functions.md). | The logic needs a library, a loop with state, or anything an expression cannot say. |
| You want the value recorded in the history and visible to later steps and queries. | You want retries, timeouts, routing to a language, or a compensation. |

A compute step can do nothing that an expression in a later step's `with` or
`when` could not. What it adds is a name: the intermediate result is computed
once, recorded in the history, and read by any number of later steps, the
`return` expression and [queries](signals.md#queries).

## Declaring a compute step

```text
step <name> { compute <expression> }
```

```orbit
task "payments.charge" {
    input { order_id: String, amount: Float }
    output { transaction_id: String }
}

workflow Checkout {
    version "1.0.0"
    input { order_id: String, amount: Float, coupon_percent: Int = 0 }

    step pricing {
        compute {
            discount: input.amount * float(input.coupon_percent) / 100.0
            needs_review: input.amount >= 1000.0
        }
    }

    step charge {
        worker "payments.charge"
        when !pricing.needs_review
        with { order_id: input.order_id, amount: input.amount - pricing.discount }
    }

    return { charged: !is_null(charge), discount: pricing.discount }
}
```

The expression can be of any type, not only a record. The compiler infers the
step's output type from it, and later steps, `return` and queries see that
type: here `pricing` is `{ discount: Float, needs_review: Bool }`, and
`step total { compute sum(input.prices) }` would make `total` a `Float`.

The expression can read `input`, `execution`, the outputs of other steps and,
in a `for` step, the loop variable. Reading a step's output makes the compute
step depend on it, exactly as in `with`. See
[Dependencies](dependencies.md).

## Members

| Member | Allowed | Notes |
|---|---|---|
| `after` | yes | Ordering, as for any step. |
| `when` | yes | Skipped steps have output `null`, and the output type becomes optional. |
| `for`, `max_parallel` | yes | One value per element; see [Fan-out](#fan-out). All elements are evaluated together, so `max_parallel` has no effect. |
| `with`, `using`, `timeout`, `queue_timeout`, `retry`, `idempotency_key` | no (`E0307`) | There is no task, no worker and no attempt to bound or retry. |
| `compensate` | no (`E0308`) | A compute step has no side effect to undo. |

```orbit
workflow Totals {
    version "1.0.0"
    input { prices: List<Float> }
    step total {
        compute sum(input.prices)
        timeout 10s
        compensate { worker "ledger.reverse" }
    }
}
```

```text
error[E0307]: `timeout` is not allowed in a `compute` step
 --> totals.orbit:6:17
  |
6 |         timeout 10s
  |                 ^^^

error[E0308]: a `compute` step has nothing to compensate
 --> totals.orbit:7:9
  |
7 |         compensate { worker "ledger.reverse" }
  |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

error: compilation failed: 2 error(s), 0 warning(s)
```

A step has exactly one action, so `compute` cannot be combined with `worker`,
`workflow`, `sleep` or `await` (`E0106`).

## How it runs

When every dependency of a compute step is done and its `when` condition is
true, the runtime evaluates the expression during the same decision and
records the value as the step's output in a `StepSucceeded` event, with
`attempt: 1`. There is no `StepScheduled` or `StepDispatched` event, no
worker, no idempotency key and no retry.

| Aspect | Behavior |
|---|---|
| Determinism | Expressions are pure: they perform no I/O and cannot read the clock or random numbers. The same input and step outputs always give the same value. |
| Replay | The value is stored in the history. Recovery and replay read it from there and never evaluate the expression again. |
| Workers | None. A compute step runs even when no worker is connected. |
| Cost | Evaluation happens inside the runtime's decision loop. Keep expressions to data mapping; do heavy work in a worker. |
| Status | `SUCCEEDED`, `SKIPPED`, `FAILED` (on an expression error) or `CANCELLED`. A compute step never stays `SCHEDULED` or `DISPATCHED`. |

In this history, `assess` is a compute step and `charge` a worker task:

```console
$ orbit logs order-A-100
+     0ms   #1  WorkflowCreated           
+     0ms   #2  WorkflowStarted           
+     1ms   #3  StepSucceeded             step=assess attempt=1
+     1ms   #4  SignalAwaitStarted        step=decision signal=review
...
+   540ms   #7  StepScheduled             step=charge attempt=1
+   547ms   #8  StepDispatched            step=charge attempt=1 worker_id=orders-python
+   556ms   #9  StepSucceeded             step=charge attempt=1
```

## Lambdas and collection functions

Collection functions take a list and a lambda, a one-parameter function
written `name => expression`:

| Function | Result |
|---|---|
| `map(list, x => expr)` | a list of `expr` for each element |
| `filter(list, x => cond)` | the elements for which `cond` is true |
| `find(list, x => cond)` | the first element for which `cond` is true, or `null` |
| `any(list, x => cond)` | `true` if `cond` is true for at least one element |
| `all(list, x => cond)` | `true` if `cond` is true for every element |
| `sum(list)` | the sum of a list of numbers |

A lambda is only allowed as the second argument of `map`, `filter`, `find`,
`any` and `all` (`E0415` elsewhere), and its parameter must not reuse the name
of a step, a loop variable, an enclosing lambda parameter, `input` or
`execution` (`E0413`). Lambdas and these functions work in every expression,
not only in compute steps. See
[Built-in functions](functions.md#collection-functions) for signatures and
details, and [Expressions](expressions.md#lambdas) for the syntax.

```orbit
schema Line {
    sku: String
    quantity: Int
    unit_price: Float
}

workflow Totals {
    version "1.0.0"
    input { lines: List<Line> }

    step totals {
        compute {
            count: len(input.lines)
            units: sum(map(input.lines, l => l.quantity))
            subtotal: sum(map(input.lines, l => float(l.quantity) * l.unit_price))
            bulk: filter(input.lines, l => l.quantity >= 10)
            first_free: find(input.lines, l => l.unit_price == 0.0)
            all_in_stock: all(input.lines, l => l.quantity > 0)
        }
    }

    step line_totals {
        for line in input.lines
        compute { sku: line.sku, total: float(line.quantity) * line.unit_price }
    }

    step per_unit {
        compute totals.subtotal / float(totals.units)
        when totals.units > 0
    }

    return {
        units: totals.units
        subtotal: totals.subtotal
        bulk_skus: map(totals.bulk, l => l.sku)
        free_sku: totals.first_free.sku
        line_totals: line_totals
        per_unit: per_unit
    }
}
```

```console
$ orbit run totals.orbit --id totals-1 --input '{"lines": [{"sku": "A", "quantity": 12, "unit_price": 2.5}, {"sku": "B", "quantity": 1, "unit_price": 0.0}]}'
deployed Totals@1.0.0 (sha256:01f95045bc38)
started execution totals-1
  ✓ line_totals                    succeeded 0ms
  ✓ line_totals[0]                 succeeded 
  ✓ line_totals[1]                 succeeded 
  ✓ per_unit                       succeeded 
  ✓ totals                         succeeded 

COMPLETED totals-1  Totals@1.0.0
  elapsed: 2ms
{
  "units": 13,
  "subtotal": 30.0,
  "bulk_skus": [
    "A"
  ],
  "free_sku": "B",
  "line_totals": [
    {
      "sku": "A",
      "total": 30.0
    },
    {
      "sku": "B",
      "total": 0.0
    }
  ],
  "per_unit": 2.3076923076923075
}
```

`first_free` has type `Line?`, because `find` may find nothing, so
`totals.first_free.sku` is a `String?`.

## Fan-out

With `for x in list`, a compute step evaluates its expression once per
element, with `x` bound to the element. As for task steps, the runtime records
`StepFanOut`, then one unit per element (`line_totals[0]`, `line_totals[1]`,
...), each with its own `StepSucceeded`, and finally the step's output: the
list of element values, in input order. The output type is `List<T>`, where
`T` is the type of the expression.

```console
+     2ms   #4  StepFanOut                step=line_totals
+     2ms   #5  StepSucceeded             step=line_totals[0] attempt=1
+     2ms   #6  StepSucceeded             step=line_totals[1] attempt=1
+     2ms   #7  StepSucceeded             step=per_unit attempt=1
+     2ms   #8  StepSucceeded             step=line_totals attempt=1
```

If any element fails, the step fails. See [Fan-out](fan-out.md).

## Conditions

A compute step with `when` is skipped when the condition is false or `null`,
and its output is `null`. Its output type is optional (`T?`, or `List<T>?`
with `for`), so readers use `coalesce` or `is_null`. In the example above,
`per_unit` is skipped for an empty order, which avoids a division by zero.
See [Conditions](conditions.md).

## Errors

The type checker reports type errors in the expression as usual, mostly
`E0400` (type mismatch), `E0405` (optional value where a value is required),
`E0406` (operator not defined) and `E0407` (unknown function or wrong number of
arguments).

Some errors can only happen at runtime: division by zero, integer overflow, a
failed conversion such as `int("abc")`, or an operation on a `JSON` value of
the wrong shape. Such an error fails the step (or the element) with the
non-retryable code `EXPRESSION_ERROR` and the message
``evaluating `compute`: <cause>``. The execution then aborts and compensates
like after any other step failure.

```orbit
workflow Discount {
    version "1.0.0"
    input { subtotal: Float, discount_percent: Int = 0 }

    step factor {
        compute input.subtotal * float(100 / (100 - input.discount_percent))
    }
}
```

```console
$ orbit run discount.orbit --id discount-1 --input '{"subtotal": 10.0, "discount_percent": 100}'
deployed Discount@1.0.0 (sha256:910bcb4107fa)
started execution discount-1
  ✗ factor                         failed evaluating `compute`: division by zero

FAILED discount-1  Discount@1.0.0
  elapsed: 1ms
  compensation: none
  error: EXPRESSION_ERROR step `factor` failed: evaluating `compute`: division by zero
```

## IR version

A workflow with a compute step, or with a lambda anywhere, compiles to
`orbit-ir/2`. See [Versioning](versioning.md#ir-version).

## See also

- [Expressions](expressions.md)
- [Built-in functions](functions.md)
- [Steps](steps.md)
- [Signals and queries](signals.md)
- [Fan-out](fan-out.md)
