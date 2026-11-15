# Workflows

A workflow declares what should happen, in which order, and with which
guarantees: its typed input, the steps that do the work, and the result it
returns. The compiler turns each workflow into an immutable, versioned
definition that the runtime executes durably. This page covers the
`workflow` declaration and each of its members.

## Declaring a workflow

```orbit
workflow OrderProcessing {
    version "1.0.0"
    description "Validate, charge and confirm one order"
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
        transaction_id: String
    }

    step validate {
        worker "orders.validate"
        with { order_id: input.order_id, amount: input.amount, items: input.items }
    }

    step charge {
        worker "payments.charge"
        after validate
        with { order_id: input.order_id, amount: input.amount, currency: input.currency }
    }

    return { status: "completed", transaction_id: charge.transaction_id }
}
```

The workflow name is an identifier. It is unique in the compilation (`E0200`)
and is what you pass to `orbit run` and to the start API.

## Members

| Member | Syntax | Required | Default | Meaning |
|--------|--------|----------|---------|---------|
| `version` | `version "1.0.0"` | yes (`E0500`) | none | The definition version. `name@version` is immutable once registered. |
| `description` | `description "..."` | no | none | Human-readable description, shown by tooling. |
| `timeout` | `timeout 5m` | no | no limit | Deadline for the whole execution, measured from its creation. |
| `input` | `input { fields }` | no | `{}` | The type of the execution input. |
| `output` | `output { fields }` | no | inferred from `return` | The type of the execution result. |
| `step` | `step name { ... }` | no | none | A unit of work. See [Steps](steps.md). |
| `signal` | `signal name { fields }` | no | none | A typed message the execution accepts; `await` steps receive it. See [Signals and queries](signals.md). |
| `query` | `query name = expression` | no | none | A typed, read-only view of the execution state, evaluated on demand. See [Signals and queries](signals.md#queries). |
| `return` | `return expression` | when `output` has required fields (`E0501`) | `{}` | The result of a successful execution. |

Members can appear in any order. `version`, `description` and `timeout`
accept an optional `:` after the keyword. `input`, `output` and `signal name`
are followed directly by a record body. A workflow can declare any number of
steps, signals and queries; step, signal and query names are separate
namespaces, each unique within the workflow (`E0302`, `E0317`, `E0318`).

The handlers `on event` and `on failure` are reserved and rejected with
`E0105` in v0.1. Use [compensation](compensation.md) to undo work after a
failure.

## Version

Every workflow declares a version. A version that is not
`MAJOR.MINOR.PATCH` (optionally with a `-pre` or `+build` suffix) produces
warning `W0201`. A missing version is an error:

```orbit
workflow Nightly {
    step pause { sleep 1h }
}
```

```text
error[E0500]: workflow `Nightly` has no version
 --> nightly.orbit:1:10
  |
1 | workflow Nightly {
  |          ^^^^^^^
  = help: add `version "1.0.0"`; running executions stay pinned to the version they started with

error: compilation failed: 1 error(s), 0 warning(s)
```

See [Versioning](versioning.md) for how versions and definition hashes
interact.

## Input

`input` declares the fields an execution starts with, using the same field
syntax as a [schema](schemas.md). Expressions read them as
`input.<field>`.

When an execution starts, the runtime:

1. treats a missing or `null` input as `{}`,
2. applies the declared defaults, recursively,
3. validates the result strictly: missing required fields, values of the wrong
   type and fields the workflow does not declare are rejected.

A rejected input fails the start request with `INVALID_INPUT`, and no
execution is created:

```bash
orbit run order.orbit --input '{"order_id": "A-1", "amount": "12"}'
```

```text
error: INVALID_INPUT (400): workflow input is invalid at $.amount: expected Float, found a string
```

Child workflows started by a `workflow` step get their defaults applied too,
but their input is validated leniently, because the parent's `with`
expression was already type-checked against the child's input.

## Output and return

`return` maps step results to the workflow result. It is evaluated once,
after every step has finished (succeeded or been skipped), and it can read
`input`, `execution` and every step.

- With `output` and `return`, the `return` expression is checked against
  `output`, with the same rules as a task input: object literals must be exact
  (`E0404`) and complete (`E0403`).
- With `output` but no `return`, the result is `{}`, which is accepted only if
  `output` has no required fields. Otherwise the compiler reports `E0501`.
- With `return` but no `output`, the output type is the inferred type of the
  `return` expression.
- With neither, the result is `{}`.

```orbit
workflow Quote {
    version "1.0.0"
    input { order_id: String, items: Int }

    step quote {
        worker "shipping.quote"
        with { order_id: input.order_id, items: input.items }
    }

    // No `output` block: the output type is inferred as
    // { carrier: String, cost: Float, expensive: Bool }.
    return { carrier: quote.carrier, cost: quote.cost, expensive: quote.cost > 50.0 }
}
```

The runtime validates the result against the output type before it completes
the execution. A failure there, or an expression error while evaluating
`return`, fails the workflow with `OUTPUT_CONTRACT_VIOLATION` or
`EXPRESSION_ERROR`. No compensation runs in that case, because every step
succeeded.

## Timeout

The workflow `timeout` is a deadline for the whole execution, counted from the
moment the execution was created. When it passes, the runtime:

1. withdraws steps that have not started,
2. waits for attempts that are in flight,
3. runs the compensations of the steps that succeeded,
4. ends the execution as `TIMED_OUT`.

Step timeouts, retries, sleeps and waits for signals all count against the
workflow deadline. A workflow `timeout` also bounds every `await` step that has
no `timeout` of its own (otherwise the compiler reports `W0606`).
Compensations run after the deadline and are not limited by it. See
[Retries and timeouts](retries-timeouts.md).

## Signals and queries

A workflow can declare the signals it accepts and the queries it answers:

```orbit
workflow Approval {
    version "1.0.0"
    timeout 14d
    input { request_id: String }

    signal review { approved: Bool, reviewer: String }

    step decision { await review }

    query status = { decided: !is_null(decision), approved: decision.approved }

    return { approved: decision.approved }
}
```

`orbit signal <id> review --data '{...}'` sends a validated payload, which the
`decision` step receives as its output. `orbit query <id> status` evaluates the
query against the current state; in a query every step output is optional,
because the step may not have run yet. A workflow that declares a signal or a
query compiles to `orbit-ir/2`. See [Signals and queries](signals.md).

## Execution lifecycle

An execution is `RUNNING` until it completes, or until it is aborted by a step
failure, a cancellation or the workflow timeout:

```text
RUNNING ──► COMPLETED
   │
   └──► COMPENSATING ──► FAILED | CANCELLED | TIMED_OUT
```

A failed, cancelled or timed-out execution records a compensation outcome:
`none` (nothing to compensate), `completed` or `failed`. See
[Compensation](compensation.md) and
[Executions and history](../runtime/executions.md).

## Execution metadata

Expressions can read three fields of the running execution:

| Expression | Type | Value |
|------------|------|-------|
| `execution.id` | `String` | the execution id |
| `execution.workflow` | `String` | the workflow name |
| `execution.version` | `String` | the workflow version |

These are often used to build idempotency keys or correlation ids:

```orbit
workflow Welcome {
    version "1.0.0"
    input { email: String }
    step send {
        worker "emails.send"
        with { to: input.email, template: "welcome" }
        idempotency_key: "welcome-" + execution.id
    }
}
```

## See also

- [Steps](steps.md)
- [Signals and queries](signals.md)
- [Schemas](schemas.md)
- [Versioning](versioning.md)
- [Executions and history](../runtime/executions.md)
- [Compensation](compensation.md)
