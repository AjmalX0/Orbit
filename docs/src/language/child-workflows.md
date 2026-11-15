# Child workflows

A `workflow` step starts another workflow as a durable child execution and
waits for its result. Child workflows let you reuse a workflow as a typed
building block: the parent's input to the child and the child's result are
checked at compile time, and the runtime links parent and child so that a
crash never starts a child twice or loses its result. This page covers the
`workflow` action, its typing, the runtime behavior, and its restrictions.

## Declaring a child workflow step

```orbit
workflow Shipping {
    version "1.0.0"
    input { order_id: String, items: Int }
    output { carrier: String, tracking: String }

    step quote { worker "shipping.quote" with { order_id: input.order_id, items: input.items } }
    step book { worker "shipping.book" with { order_id: input.order_id, carrier: quote.carrier } }

    return { carrier: quote.carrier, tracking: book.tracking }
}

workflow Fulfilment {
    version "1.0.0"
    input { order_id: String, items: List<LineItem> }

    step ship {
        workflow "Shipping"
        with { order_id: input.order_id, items: len(input.items) }
        timeout 1h
    }

    return { tracking: ship.tracking }
}
```

The name after `workflow` is a string literal naming a workflow in the same
compilation: declared in the same file or in an imported file. An unknown name
is `E0312`, with a suggestion:

```text
error[E0312]: unknown workflow `Shiping`
```

## Typing

The child's declared `input` plays the role of a task contract's input, and
its `output` (declared or inferred) plays the role of the contract's output:

- `with` is checked against the child's input. A child whose input has
  required fields needs a `with` (`E0403`).
- A reference to the step has the child's output type: `ship.tracking` is a
  `String` above.
- `when` makes the reference optional, and `for` makes it a list, exactly as
  for task steps. See [Steps](steps.md#step-output-types).

## Allowed members

| Member | Allowed | Notes |
|--------|---------|-------|
| `with` | yes | The child's input. |
| `after`, `when` | yes | |
| `for`, `max_parallel` | yes | One child per element. |
| `timeout` | yes | How long the parent waits for the child. |
| `compensate` | yes | A task that undoes the child's effect after the parent fails. |
| `using`, `retry`, `idempotency_key`, `queue_timeout` | no | `E0307` |

```orbit
workflow Child {
    version "1.0.0"
}

workflow Parent {
    version "1.0.0"
    step run {
        workflow "Child"
        retry { attempts: 3 }
    }
}
```

```text
error[E0307]: `retry` is not allowed in a `workflow` step
 --> parent.orbit:9:9
  |
9 |         retry { attempts: 3 }
  |         ^^^^^^^^^^^^^^^^^^^^^
  = note: child workflows are durable; put retries on the child's own steps

error: compilation failed: 1 error(s), 0 warning(s)
```

A child execution is durable on its own and has its own steps, retries,
timeouts and compensation. Configure reliability inside the child.

## Cycles

Workflows cannot start each other in a cycle, directly or indirectly (`E0311`):

```orbit
workflow Ping {
    version "1.0.0"
    step pong { workflow "Pong" }
}

workflow Pong {
    version "1.0.0"
    step ping { workflow "Ping" }
}
```

```text
error[E0311]: workflows call each other in a cycle
 --> ping.orbit:3:26
  |
3 |     step pong { workflow "Pong" }
  |                          ^^^^^^
8 |     step ping { workflow "Ping" }
  |                          ------ `Pong` starts `Ping`
  = note: cycle: Ping -> Pong -> Ping
  = help: a workflow cannot (directly or indirectly) start itself as a child

error: compilation failed: 1 error(s), 0 warning(s)
```

The compiler orders workflows so that every child comes before the workflows
that start it. `orbit deploy` and `orbit run` register definitions in that
order.

## Execution

When a workflow step is scheduled, the runtime evaluates `with` and starts the
child execution itself; no worker is involved.

| Aspect | Behavior |
|--------|----------|
| Child execution id | `<parent id>.<step>`, or `<parent id>.<step>.<index>` for an element of a `for` step. |
| Duplicate starts | Impossible: the id is deterministic, and starting an existing id returns the existing execution. |
| Child input | Defaults are applied; validation is lenient because the input was type-checked. |
| Child version | The exact definition the parent was compiled against, identified by its definition hash. |
| Result | When the child completes, its result becomes the step's output. |
| Tracing | The child's spans join the parent's trace, under the step. |

The runtime reconciles parents and children continuously, so a crash at any
point neither duplicates nor loses a child.

### Failure

| Child outcome | Parent step fails with |
|---------------|------------------------|
| `FAILED` | the child's own error code, with the message `` child workflow `Name` failed: ... `` |
| `CANCELLED` | `CHILD_CANCELLED` |
| `TIMED_OUT` | `CHILD_TIMED_OUT` |
| the step `timeout` passes first | `TIMEOUT`, and the child is cancelled |
| the child definition is not registered | `CHILD_NOT_REGISTERED` |
| the child cannot be started | `CHILD_START_FAILED` |

None of these is retried, because workflow steps take no retry policy. A
failed workflow step aborts the parent like any other failed step.

### Cancellation

When the parent is aborted (a step failed, it was cancelled, or it timed out),
the runtime cancels every running child of that parent. A cancelled child runs
its own compensations before it ends.

## Registering parents and children

A parent pins the definition hash of the child it was compiled against.
Registering a parent fails with `CHILD_NOT_REGISTERED` if that exact child
definition is not registered yet. `orbit deploy` registers children first.

Because the child's hash is part of the parent's definition, changing the
child changes the parent's definition hash too. After you change a child, bump
the version of every workflow that starts it; otherwise registering the parent
fails with `DEFINITION_CONFLICT`. See [Versioning](versioning.md).

## See also

- [Steps](steps.md)
- [Fan-out](fan-out.md)
- [Compensation](compensation.md)
- [Versioning](versioning.md)
- [Fan-out and child workflows guide](../guides/fulfilment.md)
