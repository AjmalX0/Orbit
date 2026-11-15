# Core concepts

This page defines the terms used throughout the ORBIT documentation and shows
how they relate. Each definition is precise about what the runtime does, so
you can rely on it when you design workflows and workers.

## Overview

```text
 .orbit source                         runtime (orbit server)
 ─────────────                         ──────────────────────
 task contract ◄─── implemented by ─── worker (any language)
      ▲                                    ▲
      │ referenced by                      │ pulls tasks for
      │                                    │
 workflow ── compiled to ──► definition ── started as ──► execution
  └─ steps                    name@version                 ├─ units (one per step,
                              + content hash               │   fan-out element or
                                                           │   compensation)
                                                           ├─ history (events)
                                                           └─ child executions
```

A **workflow** is a set of **steps**. Each step calls a **task contract**, a
child workflow, or a timer, waits for a **signal**, or computes a value. A
workflow can also declare **queries** that report its state. Compiling a workflow produces a **definition**
identified by `name@version` and a content hash. Starting a definition creates
an **execution**. The runtime splits the execution into **units**, dispatches
each task unit to a **worker** that implements the contract, and records every
transition in the execution's **history**.

## Workflow

A workflow is a named, versioned declaration of a process: typed `input`, an
optional typed `output`, a set of steps, and a `return` expression that maps
step results to the output. Workflows contain no business logic and perform no
I/O themselves. Every expression in a workflow is pure and deterministic, so
every decision the runtime makes can be reproduced from history.

```orbit
workflow Hello {
    version "1.0.0"
    input { name: String, excitement: Int = 3 }
    output { greeting: String }
    step compose { worker "greeting.compose" with { name: input.name } }
    return { greeting: compose.message }
}
```

See [Workflows](../language/workflows.md).

## Task contract

A task contract (a `task` declaration) is the typed interface between a
workflow and a worker: a name, a contract version, an input type and an output
type.

```orbit
task "greeting.compose" {
    version "1.0.0"
    input { name: String }
    output { message: String }
}
```

The compiler checks every step's `with` block against the contract's input,
and every reference to a step's output against the contract's output. The
runtime validates worker results against the output type again and fails the
attempt with `OUTPUT_CONTRACT_VIOLATION` if they do not match.

Contracts are versioned independently of workflows. A worker receives a task
only if it announced exactly that `name@version`. Names that start with
`orbit.` are reserved for built-in tasks; there is one, `orbit.http.request`.
See [Task contracts](../language/tasks.md).

## Worker

A worker is a process you run, in any language with an SDK (Python,
TypeScript, Go, Rust, Java, C#) or any program that speaks the worker
protocol. It registers with the runtime under a worker id, announces the
contracts it implements and its language runtime, long-polls for tasks, runs
your handler, and reports a result or a structured error.

Workers hold no workflow state. You can run several workers for the same
contracts to share load, and stop or restart them at any time: the runtime
redelivers work whose outcome it never learned. A step with `using python`
(or another language) is routed only to workers that registered with that
runtime. See [How workers work](../sdks/overview.md).

## Step and unit

A **step** is a named node in a workflow. It does one of five things:

| Step kind | Declared with | Executed by |
|---|---|---|
| Task step | `worker "task.name"` | a worker that implements the contract (or the runtime, for `orbit.http.request`) |
| Child workflow step | `workflow "Name"` | the runtime, which starts a child execution |
| Sleep step | `sleep 24h` | the runtime, as a durable timer |
| Await step | `await review` | the runtime, which waits for a [signal](#signal) |
| Compute step | `compute <expression>` | the runtime, which evaluates the expression (see [compute step](#compute-step)) |

A step also declares its policies: dependencies (`after`), a condition
(`when`), input (`with`), `retry`, `timeout`, `queue_timeout`,
`idempotency_key` and `compensate`. A step depends on every step named in its
`after` clause and on every step whose output it reads. Steps with no
dependency between them run concurrently.

A **unit** is what the runtime actually schedules and tracks. Most steps are
exactly one unit, keyed by the step name. Two cases create more:

| Unit key | Created for |
|---|---|
| `reserve` | an ordinary step |
| `reserve[0]`, `reserve[1]`, ... | each element of a `for` step (see [fan-out element](#fan-out-element)) |
| `compensate:reserve_inventory`, `compensate:reserve[2]` | the compensation of a succeeded unit |

Each unit has its own status, attempt counter, timeouts and idempotency key. A
unit moves through `PENDING → SCHEDULED → DISPATCHED → SUCCEEDED`, or ends
`SKIPPED`, `FAILED` or `CANCELLED`; it waits in `RETRY_WAIT` between attempts,
in `WAITING` during a sleep or while waiting for a signal, and a fanned-out step is `RUNNING` while its
elements execute. `orbit status` prints one row per unit. See
[Steps](../language/steps.md) and [Executions and history](../runtime/executions.md).

An **attempt** is one try at running a unit. `retry { attempts: 3 }` allows
three attempts in total, including the first. A redelivery after an uncertain
outcome reuses the same attempt number and does not count as a retry.

## Signal

A signal is a typed message that a person or another system sends to a
running execution, declared in the workflow with `signal <name> { fields }`.
An `await <name>` step waits for the next signal of that name, durably, and
its payload becomes the step's output. The runtime validates every payload
against the declaration before it records it, and buffers signals that arrive
before a step waits for them, delivering them in order. Send one with
`orbit signal <id> <name> --data '{...}'` or
`POST /v1/executions/{id}/signals/{name}`. See
[Signals and queries](../language/signals.md).

## Query

A query is a named, typed, read-only view of an execution's state, declared
with `query <name> = <expression>`. The runtime evaluates it on demand with
`orbit query <id> <name>` or `GET /v1/executions/{id}/queries/{name}`, against
the current input and step outputs; steps that have not succeeded read as
`null`, and the compiler types every step output as optional in a query for
that reason. A query records nothing and changes nothing. See
[Signals and queries](../language/signals.md#queries).

## Compute step

A compute step (`compute <expression>`) is a step that the runtime evaluates
itself, with no worker: a deterministic expression over the input and earlier
step outputs, whose value is recorded in the history as the step's output.
Use it for small data transformations between tasks, such as a total, a flag
or a filtered list; lambdas (`x => ...`) with `map`, `filter`, `find`, `any`
and `all` work on lists. See [Compute steps](../language/compute.md).

## Execution

An execution is one run of a definition with a specific input. It has an id
(a UUID, or a client-chosen id passed with `orbit run --id`), a status, the
state of each unit, and an output or error.

| Status | Meaning |
|---|---|
| `RUNNING` | Steps are being scheduled and executed. |
| `COMPENSATING` | The execution is aborting and running compensations. |
| `COMPLETED` | Every step finished and `return` produced the output. |
| `FAILED` | A step failed permanently. |
| `CANCELLED` | Someone cancelled the execution (`orbit cancel`). |
| `TIMED_OUT` | The workflow-level `timeout` elapsed. |

A finished execution that aborted also records `compensation: none`,
`completed` or `failed`. Starting an execution with an id that already exists
returns the existing execution instead of creating a second one.

## History and events

The history of an execution is an append-only, ordered list of events, such as
`StepScheduled`, `StepDispatched`, `StepAttemptFailed`, `RetryScheduled` and
`CompensationStarted`. History is the authoritative state. The runtime commits
events to the database before acting on them, rebuilds an execution's state
by replaying its events after a restart, and derives every decision from
history through a pure function, so the same history always leads to the same
decisions.

`orbit logs <id>` prints the history as a timeline; `orbit logs <id> --json`
prints the raw events. See [Executions and history](../runtime/executions.md).

## Definition, version and hash

Compiling a workflow produces a definition in the intermediate representation
(`orbit-ir/1`, or `orbit-ir/2` for workflows that use signals, queries,
`await` or `compute` steps, or lambdas). Registering it with the runtime (`orbit deploy`, or implicitly
`orbit run`) stores it under its `name@version`, together with a SHA-256
`definition_hash` of its semantic content.

- Source locations and formatting do not affect the hash, so reformatting a
  file changes nothing.
- Registering the same content again is a no-op (`up to date`).
- Registering different content under an existing `name@version` is rejected
  with `DEFINITION_CONFLICT`. To change a workflow, change its version.
- Each execution is pinned to the hash it started with. Deploying a new
  version never changes running executions.

See [Versioning](../language/versioning.md).

## Idempotency key

Every task carries an idempotency key: a string that identifies the unit of
work and stays the same across retries and redeliveries. By default it is
`<execution_id>/<unit>`, for example
`99e811dd-7448-41cc-ac28-bc58d50200d2/compose` or `order-A-4/reserve[1]`. You
can set your own with `idempotency_key:`, such as
`"charge-" + input.order_id`, to deduplicate across executions.

The runtime evaluates the key once, when the unit is scheduled. Because
delivery is at least once, a worker can receive the same task more than once;
workers that cause side effects should use the key to perform them only once.
See [Idempotency](../language/idempotency.md) and
[Delivery guarantees](../runtime/guarantees.md).

## Compensation

A compensation is the business action that undoes a succeeded step, declared
in the step's `compensate` block. When an execution fails, is cancelled or
times out, the runtime:

1. withdraws work that has not started,
2. waits for in-flight attempts to finish (they may still succeed),
3. runs the compensation of every succeeded unit that has one, one at a time,
   newest first.

Compensations are ordinary units (`compensate:<unit>`) with their own retries,
timeouts and idempotency keys. They are explicit business logic, such as a
refund or a release of reserved stock, not a database rollback. See
[Compensation](../language/compensation.md).

## Outcome handler

An outcome handler is a block of steps that runs once compensation has
finished, when an execution fails (`on failure`), is cancelled (`on cancel`)
or times out (`on timeout`), before the terminal status is recorded. Handler
steps read a `failure` value (`reason`, `code`, `message`) and the outputs of
the workflow steps that succeeded. Their units are `on_<kind>:<step>`, for
example `on_failure:tell_customer`. A failing handler step does not change
the execution's outcome. See [Outcome handlers](../language/handlers.md).

## Fan-out element

A `for` step (`for item in input.items`) runs its task once per list element.
Each element is a separate unit, `step[i]`, with its own retries, timeouts and
idempotency key (`<execution>/step[i]`). At most `max_parallel` elements run at
the same time. The step's output is the list of element outputs in input
order. The step succeeds when every element succeeds and fails when any
element fails permanently; on compensation, each element that succeeded is
compensated individually. See [Fan-out](../language/fan-out.md).

## Child execution

A `workflow "Name"` step starts another workflow as a child execution. The
child has a deterministic id, `<parent>.<step>` (or `<parent>.<step>.<i>`
inside a `for` step), so a crash or restart never starts it twice. The child is
a full execution with its own history; `orbit ps` lists it and
`orbit status <parent>.<step>` shows it. Its output becomes the step's output.
If the child fails, is cancelled or times out, the step fails, and cancelling
the parent cancels its running children. See
[Child workflows](../language/child-workflows.md).

## How they relate

| Concept | Belongs to | Identified by |
|---|---|---|
| Task contract | a compilation (any `.orbit` file) | `name@version` |
| Workflow | a compilation | `name` and `version` |
| Definition | the runtime | `name@version` and `definition_hash` |
| Execution | a definition | execution id |
| Unit | an execution | unit key (`step`, `step[i]`, `compensate:step`, `on_failure:step`) |
| Signal | an execution's buffer | name, and the sequence number of its `SignalReceived` event |
| Query | a workflow | name |
| Attempt | a unit | attempt number |
| Event | an execution's history | sequence number (`#1`, `#2`, ...) |
| Child execution | a parent unit | `<parent>.<step>` |
| Worker | the runtime's worker registry | worker id |

## See also

- [Quick start](quickstart.md)
- [Steps](../language/steps.md)
- [Signals and queries](../language/signals.md)
- [Outcome handlers](../language/handlers.md)
- [Delivery guarantees](../runtime/guarantees.md)
- [Executions and history](../runtime/executions.md)
- [Worker protocol](../reference/protocol.md)
