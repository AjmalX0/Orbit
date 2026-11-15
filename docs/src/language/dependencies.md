# Dependencies

Steps in a workflow form a directed acyclic graph. The compiler builds the
graph from explicit `after` lists and from the data each step reads, rejects
cycles, and fixes a deterministic order. The runtime starts a step as soon as
everything it depends on has finished, and runs independent steps
concurrently. This page explains how dependencies are formed and checked.

## Explicit dependencies: `after`

`after` lists steps that must finish before this step starts:

```orbit
step notify {
    worker "notifications.send"
    after validate_order, reserve_inventory
    with { customer_id: input.customer_id, message: "Order received" }
}
```

Use `after` when there is an ordering requirement that the data does not
express, for example "send the confirmation only after the inventory is
reserved". You can repeat `after` in a step; the lists are combined.

## Implicit dependencies: data references

Reading a step's output makes the reading step depend on it. You do not need
to repeat that dependency in `after`. References count in these members:

| Member | Creates a dependency |
|--------|----------------------|
| `with` | yes |
| `when` | yes |
| `idempotency_key` | yes |
| `for ... in` (the list expression) | yes |
| `compensate { ... }` | no; a compensation can only read the step and its existing dependencies |
| workflow `return` | no; `return` runs after every step has finished |

In this workflow, `charge` depends on `risk` because its `when` reads
`risk.score`, and on `reserve` through `after`:

```orbit
workflow Checkout {
    version "1.0.0"
    input { order_id: String, customer_id: String, amount: Float, items: List<LineItem> }

    step risk {
        worker "risk.analyze"
        with { customer_id: input.customer_id, amount: input.amount }
    }

    step reserve {
        worker "inventory.reserve"
        with { order_id: input.order_id, items: input.items }
    }

    step charge {
        worker "payments.charge"
        after reserve
        when risk.score < 0.8
        with { order_id: input.order_id, amount: input.amount, currency: "USD" }
    }

    return { reservation_id: reserve.reservation_id, transaction_id: charge.transaction_id }
}
```

`risk` and `reserve` have no dependency on each other, so they run at the same
time. `charge` starts when both have finished.

## When a step becomes eligible

A step is scheduled when all of these hold:

1. every direct dependency has finished as `SUCCEEDED` or `SKIPPED`,
2. the workflow is not being aborted (no step has failed permanently, and the
   execution has not been cancelled or timed out),
3. its `when` condition, if any, is true.

A skipped dependency counts as finished. Its output is `null`, which is why
references to a conditional step are optional. See
[Conditions](conditions.md).

A failed dependency never lets a step start: the failure aborts the workflow,
and pending steps are withdrawn (`CANCELLED`). See
[Compensation](compensation.md).

## Concurrency

There is no implicit sequencing by source order. Steps without a path between
them in the graph run concurrently, limited only by available workers. To run
steps one after another, connect them with `after` or a data reference.

## Rules the compiler enforces

| Code | Rule |
|------|------|
| `E0303` | Every name in `after` is a step of the same workflow. |
| `E0305` | A step cannot list itself in `after` or read its own output. |
| `E0304` | The graph has no cycles. |
| `E0401` | A referenced name is a step, `input`, `execution` or a loop variable. |

An unknown `after` target suggests the closest step name:

```orbit
task "shipping.quote" {
    input { order_id: String }
    output { carrier: String }
}

task "shipping.book" {
    input { order_id: String, carrier: String }
    output { tracking: String }
}

workflow Ship {
    version "1.0.0"
    input { order_id: String }
    step quote { worker "shipping.quote" with { order_id: input.order_id } }
    step book {
        worker "shipping.book"
        after qoute
        with { order_id: input.order_id, carrier: quote.carrier }
    }
}
```

```text
error[E0303]: `after` references unknown step `qoute`
  --> ship.orbit:17:15
   |
17 |         after qoute
   |               ^^^^^
   = help: did you mean `quote`?

error: compilation failed: 1 error(s), 0 warning(s)
```

A cycle is reported with the full path and a label at each edge:

```orbit
task "a.run" {
    input { n: Int }
    output { v: Int }
}

workflow Loop {
    version "1.0.0"
    step first { worker "a.run" with { n: second.v } }
    step second { worker "a.run" with { n: first.v } }
}
```

```text
error[E0304]: circular dependency between steps
 --> loop.orbit:8:10
  |
8 |     step first { worker "a.run" with { n: second.v } }
  |          ^^^^^
  |                                           ------ `first` depends on `second`
9 |     step second { worker "a.run" with { n: first.v } }
  |                                            ----- `second` depends on `first`
  = note: cycle: first -> second -> first
  = help: the step graph must be acyclic; remove one of these dependencies

error: compilation failed: 1 error(s), 0 warning(s)
```

Reading a step's own output, for example to "increment" a value, is `E0305`.
Use two steps, or a compensation, which may read its own step.

## Deterministic order

The compiler sorts steps topologically and breaks ties by source order. The
resulting order is stored in the compiled IR, together with each step's
sorted dependency list, and the runtime evaluates steps in that order. The
order, and therefore every scheduling decision, is the same on every replay.

To see the graph, run `orbit graph`, which renders Mermaid or Graphviz. See
[Workflow graphs](../tooling/graph.md).

## See also

- [Steps](steps.md)
- [Conditions](conditions.md)
- [Workflow graphs](../tooling/graph.md)
- [Executions and history](../runtime/executions.md)
