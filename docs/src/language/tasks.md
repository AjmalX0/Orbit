# Task contracts

A task contract declares a unit of work that workers implement: its name, its
version, and the types of its input and output. Workflows call tasks by name,
and the compiler checks every call against the contract. Workers in any
language register against the same contract. This page covers the `task`
declaration, how the compiler uses it, and how the runtime enforces it.

## Declaring a task

```orbit
schema PaymentRequest {
    order_id: String
    amount: Float
    currency: String = "USD"
}

task "payments.charge" {
    version "1.0.0"
    description "Charge a customer's card"
    input PaymentRequest                    // a schema...
    output { transaction_id: String }       // ...or an inline record
}
```

The name is a string literal. By convention it is a dotted, lowercase
`service.action` name, but any string is allowed except names starting with
`orbit.`, which are reserved for built-in tasks.

## Members

| Member | Type | Default | Meaning |
|--------|------|---------|---------|
| `version` | string | `"1.0.0"` | The contract version. Workers receive only tasks whose name and contract version they announced. A version that is not `MAJOR.MINOR.PATCH` produces warning `W0201`. |
| `description` | string | none | Human-readable description, shown by tooling. |
| `input` | record type | `{}` | The type of the value the workflow passes in `with`. |
| `output` | record type | `{}` | The type of the value the worker returns. |
| `compensated_by` | string | none | The task that undoes this one. Steps that call this task without a compensation that calls the named task get a warning (`W0601`, `W0602`). See [Declaring how a task is undone](#declaring-how-a-task-is-undone). |

Members can appear in any order, each may be followed by an optional `:`
(`version: "1.0.0"`), and separators between them are optional.

`input` and `output` must be record types: a schema name, an inline record, or
`JSON`. Any other type is `E0310`:

```orbit
task "math.square" {
    input Int
    output { value: Int }
}
```

```text
error[E0310]: task input must be a record type, found `Int`
 --> square.orbit:2:11
  |
2 |     input Int
  |           ^^^

error: compilation failed: 1 error(s), 0 warning(s)
```

Wrap scalar values in a record (`input { value: Int }`). A task with no
`input` takes no input, so a step can call it without `with`.

## Rules the compiler enforces

| Code | Rule |
|------|------|
| `E0200` | A task name is declared once in the compilation, including imported files. |
| `E0200` | A task name cannot start with `orbit.`. |
| `E0310` | `input` and `output` are record types or `JSON`. |
| `W0201` | `version` is `MAJOR.MINOR.PATCH`, optionally with `-pre` or `+build` suffixes. |
| `E0300` | A step's `worker "name"` refers to a declared task. The diagnostic suggests the closest name. |
| `E0316` | `compensated_by` names a declared task. The diagnostic suggests the closest name. |
| `W0601` | A step that calls a task with `compensated_by` has a `compensate` block. |
| `W0602` | That `compensate` block calls the task named by `compensated_by`. |

## Declaring how a task is undone

`compensated_by` records, on the contract, which task reverses its effect:

```orbit
task "inventory.reserve" {
    version "1.0.0"
    input { order_id: String, quantity: Int }
    output { reservation_id: String }
    compensated_by "inventory.release"
}

task "inventory.release" {
    version "1.0.0"
    input { reservation_id: String }
    output { released: Bool }
}

workflow Reserve {
    version "1.0.0"
    input { order_id: String, quantity: Int }

    step reserve {
        worker "inventory.reserve"
        with { order_id: input.order_id, quantity: input.quantity }
        compensate {
            worker "inventory.release"
            with { reservation_id: reserve.reservation_id }
        }
    }
}
```

The contract author knows which tasks have effects that must be undone; the
compiler then warns every workflow author who calls the task without the
matching compensation. The runtime does not use `compensated_by`: a step is
compensated only by its own `compensate` block. Like the rest of the contract,
`compensated_by` is part of the definition hash of every workflow that uses
the task. See [Static safety checks](safety.md).

## How the compiler uses a contract

When a step runs a task with `worker "payments.charge"`:

- The step's `with` expression is checked against the contract `input`. Object
  literals must provide every required field (`E0403`) and must not contain
  unknown fields (`E0404`). See [Types](types.md#compatibility-rules).
- A step without `with` is accepted only if the contract input has no required
  fields. Otherwise the compiler reports `E0403` and lists the missing fields.
- The contract `output` is the type other expressions see when they reference
  the step, for example `charge.transaction_id: String`.

```orbit
task "payments.charge" {
    version "1.0.0"
    input { order_id: String, amount: Float }
    output { transaction_id: String }
}

workflow Pay {
    version "1.0.0"
    input { order_id: String, amount: Float }

    step charge {
        worker "payments.charge"
        with { order_id: input.order_id, amount: input.amount }
    }

    return { transaction_id: charge.transaction_id }
}
```

The compiled IR embeds every contract a workflow uses, so the runtime does not
need the source files.

## How the runtime enforces a contract

The runtime validates the data on both sides of every task:

1. **Input.** When the step is scheduled, the runtime evaluates `with`,
   applies the contract's input defaults, and validates the result. Unknown
   fields are allowed. An invalid value fails the step with the non-retryable
   error `INPUT_CONTRACT_VIOLATION`.
2. **Output.** When a worker reports success, the runtime applies the output
   defaults and validates the result. Unknown fields are allowed, so a worker
   can add fields without breaking older workflows. An invalid result fails
   the attempt with `OUTPUT_CONTRACT_VIOLATION`, which is not retried. A
   `null` result counts as `{}`.

Each task is routed to a worker that announced the same task name and the same
contract version. A step's `using <language>` narrows that further to workers
whose runtime matches (case-insensitively); `using any` matches every worker.
See [How workers work](../sdks/overview.md).

## Changing a contract

A contract is part of every workflow definition that uses it, and it
contributes to the workflow's definition hash. Changing a contract, even only
its output, changes the hash of every workflow that uses it, so you must bump
those workflows' versions before you register them again. Bump the contract
version when the change is incompatible for workers, so that old workers stop
receiving the task. See [Versioning](versioning.md).

## Built-in tasks

Built-in tasks run inside the runtime instead of on a worker. ORBIT v0.1 has
one, `orbit.http.request`, declared in the prelude that every compilation
includes:

```orbit
workflow Ping {
    version "1.0.0"
    step probe {
        worker "orbit.http.request"
        with { url: "https://status.example.com/health" }
    }
    return { status: probe.status }
}
```

See [HTTP adapter](http-adapter.md).

## See also

- [Schemas](schemas.md)
- [Steps](steps.md)
- [Compensation](compensation.md)
- [Static safety checks](safety.md)
- [Versioning](versioning.md)
- [How workers work](../sdks/overview.md)
- [Worker protocol](../reference/protocol.md)
