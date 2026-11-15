# Fan-out

A `for` step runs once per element of a list, as independent units that run
in parallel up to an optional limit. Each element has its own retries,
timeouts, idempotency key and compensation, and the step's output is the list
of element outputs in input order. This page covers the `for` member,
`max_parallel`, typing, failure behavior and compensation of fan-out steps.

## Declaring a fan-out step

```orbit
workflow ReserveAll {
    version "1.0.0"
    input { order_id: String, items: List<LineItem> }

    step reserve {
        for item in input.items
        max_parallel 4
        worker "inventory.reserve"
        with { order_id: input.order_id, items: [item] }
        compensate {
            worker "inventory.release"
            with { reservation_id: reserve.reservation_id }   // this element's output
        }
    }

    return { reservations: reserve }
}
```

`for <name> in <expression>` binds a loop variable to each element. It has no
optional `:`. The other step members (`worker` or `workflow`, `with`,
`retry`, `timeout`, ...) describe what happens for one element.

## The list expression

The expression after `in` must be a list:

| Type of the expression | Loop variable type | Notes |
|------------------------|--------------------|-------|
| `List<T>` | `T` | |
| `List<T>?` | `T` | `null` is treated as an empty list. |
| `JSON` | `JSON` | Anything other than an array at runtime fails the step with `EXPRESSION_ERROR`. |
| anything else | | `E0414` |

The list can come from the input, a previous step (which becomes a
dependency), or a literal. It is evaluated once, when the step becomes
eligible, and the evaluated list is stored in the history (`StepFanOut`). The
number and content of the elements are therefore fixed, even across restarts.

## The loop variable

The loop variable is in scope in `with`, `idempotency_key` and `compensate`. It
is not in scope in `when`, which decides once whether the whole step runs
(`E0401`). See [Conditions](conditions.md#conditions-and-fan-out).

The variable name cannot be the name of a step in the workflow, or `input` or
`execution` (`E0413`):

```text
error[E0413]: loop variable `input` shadows a built-in name
```

## Concurrency: `max_parallel`

`max_parallel N` caps how many elements are in progress at once: scheduled,
running, waiting for a retry, or waiting for redelivery. When an element
finishes, the next pending element (in list order) starts.

| Property | Value |
|----------|-------|
| Default | no limit: all elements are scheduled at once |
| Range | integer literal from 1 to 10000 (`E0410` otherwise) |
| Without `for` | `E0307`: `` `max_parallel` only applies to `for` steps `` |

## Units and keys

Each element is its own unit, keyed `<step>[<index>]` with a zero-based index:
`reserve[0]`, `reserve[1]`, and so on. Each unit has:

- its own input, evaluated with the loop variable bound to that element,
- its own attempts and retry delays, following the step's `retry` policy,
- its own `timeout` and `queue_timeout`,
- its own idempotency key, `<execution id>/reserve[<index>]` by default (see
  [Idempotency](idempotency.md#keys-in-fan-out-steps)),
- for a `workflow` step, its own child execution, `<parent id>.reserve.<index>`.

`orbit status` lists the element units under the step.

## Output

The step succeeds when every element succeeds. Its output is the list of the
element outputs, in the order of the input list, regardless of the order in
which elements finished.

| Step | Type of a reference to it |
|------|---------------------------|
| `for` over a task with output `O` | `List<O>` |
| `for` over a child workflow with output `O` | `List<O>` |
| `for` with `when` | `List<O>?` |

You can index the result like any list. Indexing is optional-typed, because the
index may be out of range:

```orbit
return {
    count: len(reserve)
    first_reservation: reserve[0].reservation_id     // String?
}
```

An empty list succeeds immediately with the output `[]`, without contacting
any worker.

## Failure

When an element fails permanently, after its own retries:

1. the step fails with that element's error,
2. the workflow is aborted: elements that have not started are withdrawn,
   elements in flight are awaited,
3. compensation runs for every element that succeeded.

There is no partial success: one failed element fails the step. To tolerate
individual failures, have the worker report a result that describes the
failure instead of an error.

## Compensation

A `compensate` block on a `for` step runs once per element that succeeded,
newest first, together with the other compensations of the workflow. Inside
the block, a reference to the step (`reserve.reservation_id` above) is the
output of the element being compensated, not the list, and the loop variable
is bound to that element. Each element's compensation is a unit keyed
`compensate:reserve[<index>]`. See [Compensation](compensation.md).

## Combining with other members

| Member | Effect on a `for` step |
|--------|------------------------|
| `worker "t"` | One task per element. |
| `workflow "W"` | One child execution per element. |
| `sleep` | Not allowed (`E0307`). |
| `await` | Not allowed (`E0307`): an `await` step receives one signal. |
| `compute` | One value per element, evaluated by the runtime. See [Compute steps](compute.md#fan-out). |
| `when` | Evaluated once, before fan-out. |
| `after` | The whole step waits; all elements start after the dependencies. |
| `retry`, `timeout`, `queue_timeout` | Apply to each element separately. |
| `idempotency_key` | Evaluated per element. |

## Filtering

`for` has no per-element filter clause (such as `for x in xs where ...`). To
process only some elements, iterate over a filtered list, written with the
built-in `filter` and a lambda:

```orbit
import "contracts.orbit"

workflow ReserveInStock {
    version "1.0.0"
    input { order_id: String, items: List<LineItem> }

    step reserve {
        for item in filter(input.items, i => i.quantity > 0)
        worker "inventory.reserve"
        with { order_id: input.order_id, items: [item] }
    }
}
```

A lambda makes the workflow compile to `orbit-ir/2` ([Versioning](versioning.md)). See [Built-in functions](functions.md).

## See also

- [Steps](steps.md)
- [Child workflows](child-workflows.md)
- [Compensation](compensation.md)
- [Idempotency](idempotency.md)
- [Fan-out and child workflows guide](../guides/fulfilment.md)
