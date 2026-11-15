# Steps

A step is one unit of work in a workflow: a task run by a worker, a child
workflow, or a durable timer. Each step declares what it does, what it needs,
and the policies that apply to it. This page is the reference for the `step`
block: its actions, every member, where each member is allowed, and the type
other expressions see when they reference the step.

## Anatomy of a step

```orbit
step process_payment {
    worker "payments.charge"          // the action: a task contract
    using python                      // only route to Python workers
    after reserve_inventory           // explicit ordering
    when analyze_risk.score < 0.8     // run only if true, otherwise skip
    with {                            // the task input, checked against the contract
        order_id: input.order_id
        amount: input.amount
        currency: input.currency
    }
    idempotency_key: "charge-" + input.order_id
    timeout 30s                       // per attempt
    queue_timeout 2m                  // fail if no worker picks it up in time
    retry {
        attempts: 5                   // total attempts, including the first
        backoff: exponential
        initial_delay: 500ms
        max_delay: 10s
        non_retryable: ["CARD_DECLINED"]
    }
    compensate {
        worker "payments.refund"
        with { transaction_id: process_payment.transaction_id, amount: input.amount }
    }
}
```

The step name is an identifier. It is unique within the workflow (`E0302`), and
expressions use it to read the step's output (`process_payment.transaction_id`).
Members can appear in any order, and separators between them are optional.

## Actions

Every step has exactly one action:

| Action | Syntax | What runs |
|--------|--------|-----------|
| Task | `worker "task.name"` | A task, executed by a worker that implements the contract, or by the runtime for built-in tasks. |
| Child workflow | `workflow "Name"` | Another workflow, as a durable child execution. See [Child workflows](child-workflows.md). |
| Timer | `sleep 10s` | A durable timer. See [Durable sleep](sleep.md). |

A step without an action is `E0301`. A step with two actions is `E0106`:

```orbit
task "reports.build" {
    output { url: String }
}

workflow Report {
    version "1.0.0"
    step build {
        worker "reports.build"
        sleep 10s
    }
}
```

```text
error[E0106]: a step can only have one `worker`, `workflow` or `sleep`
 --> report.orbit:9:9
  |
9 |         sleep 10s
  |         ^^^^^
  = help: split the work into separate steps

error: compilation failed: 1 error(s), 0 warning(s)
```

## Members

| Member | Syntax | Default | Meaning | Page |
|--------|--------|---------|---------|------|
| `worker` | `worker "name"` | | Run a task. | [Task contracts](tasks.md) |
| `workflow` | `workflow "Name"` | | Start a child workflow. | [Child workflows](child-workflows.md) |
| `sleep` | `sleep DURATION` | | Wait durably. | [Durable sleep](sleep.md) |
| `with` | `with EXPR` | `{}` | The input passed to the task or child workflow. | below |
| `using` | `using LANGUAGE` | any worker | Route the task only to workers of this runtime. | below |
| `after` | `after a, b` | none | Run after these steps. | [Dependencies](dependencies.md) |
| `when` | `when EXPR` | always run | Run only if the condition is true; otherwise skip. | [Conditions](conditions.md) |
| `for` | `for x in EXPR` | none | Run once per list element. | [Fan-out](fan-out.md) |
| `max_parallel` | `max_parallel N` | unlimited | Concurrency limit for a `for` step, 1 to 10000. | [Fan-out](fan-out.md) |
| `timeout` | `timeout DURATION` | no limit | Limit for one attempt, from dispatch to result. | [Retries and timeouts](retries-timeouts.md) |
| `queue_timeout` | `queue_timeout DURATION` | no limit | Limit for waiting for a worker. | [Retries and timeouts](retries-timeouts.md) |
| `retry` | `retry { ... }` | one attempt | Retry policy. | [Retries and timeouts](retries-timeouts.md) |
| `idempotency_key` | `idempotency_key EXPR` | `<execution id>/<unit>` | The key workers use to deduplicate side effects. | [Idempotency](idempotency.md) |
| `compensate` | `compensate { ... }` | none | The action that undoes this step. | [Compensation](compensation.md) |

Syntax notes:

- Every member except `retry`, `compensate` and `for` accepts an optional `:`
  after the keyword: `timeout: 30s` is the same as `timeout 30s`. `retry` and
  `compensate` are followed directly by `{`, and `retry: {` is a syntax error.
- `DURATION` is a duration literal such as `30s`. Expressions are not
  accepted where a duration is required.
- `after` takes a comma-separated list of step names. You can repeat `after`;
  the lists are combined.
- Every other member may appear at most once per step (`E0106`).
- An unknown member is `E0102`, with a suggestion for close misspellings:

  ```orbit
  task "reports.build" {
      output { url: String }
  }

  workflow Report {
      version "1.0.0"
      step build {
          wroker "reports.build"
      }
  }
  ```

  ```text
  error[E0301]: step `build` does nothing
   --> report.orbit:7:10
    |
  7 |     step build {
    |          ^^^^^
    = help: add `worker "task.name"`, `workflow "Name"` or `sleep 10s`

  error[E0102]: unexpected `wroker` in step
   --> report.orbit:8:9
    |
  8 |         wroker "reports.build"
    |         ^^^^^^
    = help: did you mean `worker`?

  error: compilation failed: 2 error(s), 0 warning(s)
  ```

## Where members are allowed

Not every member makes sense for every kind of step. The compiler rejects
misplaced members with `E0307` (or `E0308` for compensation):

| Member | Task step | Workflow step | Sleep step | `compensate` block |
|--------|:---------:|:-------------:|:----------:|:------------------:|
| `with` | yes | yes | no | yes |
| `using` | yes | no | no | yes |
| `after` | yes | yes | yes | no |
| `when` | yes | yes | yes | no |
| `for` | yes | yes | no | no |
| `max_parallel` | with `for` | with `for` | no | no |
| `timeout` | yes | yes | no | yes |
| `queue_timeout` | yes | no | no | yes |
| `retry` | yes | no | no | yes |
| `idempotency_key` | yes | no | no | yes |
| `compensate` | yes | yes | no (`E0308`) | not allowed (`E0102`) |

A `compensate` block must call a task with `worker`; `sleep` or `workflow`
there is `E0308`.

## Step names

Step names are identifiers. Some names are reserved because expressions use
them for other purposes, and declaring a step with one of them is `E0306`:

`input`, `execution`, `output`, `if`, `then`, `else`

## Input: `with`

`with` is an expression whose value becomes the task input (or the child
workflow input). It is usually an object literal, but any expression of a
compatible type works:

```orbit
schema PaymentRequest {
    order_id: String
    amount: Float
    currency: String
}

task "payments.authorize" {
    input PaymentRequest
    output { authorization_id: String }
}

workflow Authorize {
    version "1.0.0"
    input { payment: PaymentRequest, note: String? }

    // Pass a whole record from the input.
    step authorize { worker "payments.authorize" with input.payment }
}
```

The expression is checked against the contract input (see
[Types](types.md#compatibility-rules)). It is evaluated once, when the step is
scheduled, and the result is stored in the execution history. Retries and
redeliveries reuse the stored value. If evaluation fails at runtime, for
example because of a division by zero, the step fails with the non-retryable
error `EXPRESSION_ERROR`.

A step without `with` passes `{}`. That is accepted only when the contract
input has no required fields; otherwise the compiler reports `E0403`.

Reading another step's output in `with` makes this step depend on that step.
See [Dependencies](dependencies.md).

## Routing: `using`

`using` restricts which workers receive the task: only workers that announce
the given runtime are eligible. The value is an identifier, compared
case-insensitively with the runtime a worker announces. `using any` places no
restriction.

The compiler knows these runtime names: `python`, `typescript`, `javascript`,
`go`, `rust`, `java`, `kotlin`, `csharp`, `ruby`, `php`, `elixir`, `swift`,
`cpp`, `http` and `any`. Any other name produces warning `W0301`, with a
suggestion when the name is close to a known one:

```orbit
task "risk.analyze" {
    input { customer_id: String }
    output { score: Float }
}

workflow Risk {
    version "1.0.0"
    input { customer_id: String }
    step score {
        worker "risk.analyze"
        using pyhton
        with { customer_id: input.customer_id }
    }
}
```

```text
warning[W0301]: unknown worker language `pyhton`
  --> risk.orbit:11:15
   |
11 |         using pyhton
   |               ^^^^^^
   = help: did you mean `python`?
```

The warning does not stop compilation, but a misspelled runtime means no
worker ever receives the task.

## Step output types

Expressions refer to a step by name. The type they see depends on the step's
action and on `when` and `for`:

| Step | Type of `name` |
|------|----------------|
| `worker "t"` | the output type of task `t` |
| `workflow "W"` | the output type of workflow `W` |
| `sleep d` | `{}` |
| with `when` | the type above, made optional (`T?`) |
| with `for` | `List<T>`, one element per list item |
| with `for` and `when` | `List<T>?` |

Inside the step's own `compensate` block, the step's output is never optional,
and for a `for` step it is the output of the element being compensated. See
[Compensation](compensation.md#what-a-compensation-can-read).

## Units and status

At runtime each step is one or more *units*. A plain step is one unit, keyed by
its name. A `for` step has one unit per element, keyed `name[0]`, `name[1]`,
and so on. A compensation is a unit keyed `compensate:<unit>`. Unit keys appear
in `orbit status`, `orbit logs`, idempotency keys and child execution ids.

A unit moves through these states:

| State | Meaning |
|-------|---------|
| `PENDING` | Waiting for dependencies. |
| `SCHEDULED` | Input evaluated, queued for a worker. |
| `DISPATCHED` | A worker (or the runtime) is executing the attempt. |
| `RETRY_WAIT` | A retry is scheduled after a backoff delay. |
| `WAITING` | A `sleep` timer is running. |
| `RUNNING` | A `for` step whose elements are in progress. |
| `SUCCEEDED` | Finished successfully. |
| `SKIPPED` | Its `when` condition was false or `null`. |
| `FAILED` | Failed permanently. |
| `CANCELLED` | Withdrawn because the workflow was aborted. |

See [Executions and history](../runtime/executions.md) for the events behind
these transitions.

## See also

- [Dependencies](dependencies.md)
- [Conditions](conditions.md)
- [Retries and timeouts](retries-timeouts.md)
- [Fan-out](fan-out.md)
- [Compensation](compensation.md)
- [Compiler diagnostics](../reference/diagnostics.md)
