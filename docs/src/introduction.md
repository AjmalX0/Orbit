# Introduction

ORBIT is a declarative workflow language and a durable runtime for
coordinating work across services written in different languages. You
describe what should happen in a `.orbit` file: which tasks run, in which
order, with which retries, timeouts and compensations. The compiler checks the
whole workflow against typed task contracts before anything runs, and the
runtime executes it durably, recording every decision as an event. Business
logic stays in ordinary programs, called workers, written in Python,
TypeScript, Go, Rust, Java or C#.

This page explains the problem ORBIT addresses, how its parts fit together,
and what it guarantees.

## The problem

A business process such as "validate an order, score its risk, reserve stock,
charge the card, notify the customer" usually spans several services, often in
several languages. Coordinating them by hand means writing the same
infrastructure in every service: retry loops, timeouts, deduplication, state
tables to survive restarts, and cleanup code for when a later step fails. That
coordination logic ends up scattered, hard to review, and hard to reason about
after a crash.

ORBIT moves the coordination into one place. The workflow file states the
process and its policies. The runtime owns all workflow state and makes it
survive crashes. Workers only implement individual tasks.

## How the pieces fit

```text
  order.orbit ──► orbit compiler ──► orbit-ir (typed, hashed JSON)
                   type checks,              │
                   dependency graph          │ orbit deploy / orbit run
                                             ▼
                                   ┌───────────────────┐
                                   │   orbit server    │  SQLite or PostgreSQL
                                   │ (durable runtime) │──► event history
                                   └─────────┬─────────┘
                       worker protocol (HTTP/JSON or gRPC)
                 ┌──────────┬───────────┼──────────┬──────────┐
              Python   TypeScript      Go        Rust     Java, C#
              worker     worker      worker     worker    workers
```

| Part | What it does |
|---|---|
| Language | Declares schemas, task contracts and workflows in `.orbit` files, including the typed signals a workflow accepts and the queries it answers. |
| Compiler | Resolves imports and names, type-checks every step input and reference, builds the dependency graph, rejects cycles, warns about missing compensations and other likely mistakes, and lowers the result to the IR. It runs inside the `orbit` CLI (`orbit check`, `orbit compile`). |
| IR | `orbit-ir/1` and `orbit-ir/2`: a versioned JSON representation of a workflow, identified by a SHA-256 content hash. The runtime only accepts IR, never source. |
| Runtime | `orbit server`: stores definitions and executions, decides what runs next, dispatches tasks to workers, evaluates compute steps, delivers signals, answers queries, applies retries, timeouts and compensations, and recovers after a crash. |
| Workers | Your processes. Each registers the task contracts it implements, long-polls the runtime for tasks, and reports results or structured errors through an SDK. |

The CLI is a single binary, `orbit`, that contains the compiler, the runtime
server, the formatter, the language server and the client commands.

## A short example

A task contract declares the typed interface that a worker implements. A
workflow composes tasks into steps:

```orbit
task "greeting.compose" {
    version "1.0.0"
    input { name: String }
    output { message: String }
}

task "greeting.decorate" {
    version "1.0.0"
    input { message: String, excitement: Int }
    output { message: String }
}

workflow Hello {
    version "1.0.0"

    input {
        name: String
        excitement: Int = 3
    }

    output { greeting: String }

    step compose {
        worker "greeting.compose"
        using python
        with { name: input.name }
        retry { attempts: 3, backoff: exponential, initial_delay: 500ms }
    }

    step decorate {
        worker "greeting.decorate"
        using typescript
        with { message: compose.message, excitement: input.excitement }
        timeout 10s
    }

    return { greeting: decorate.message }
}
```

`decorate` reads `compose.message`, so it depends on `compose` and runs after
it. `compose` is served by a Python worker and retried up to three times;
`decorate` is served by a TypeScript worker and limited to 10 seconds per
attempt. If `decorate` passed `compose.messsage` (a typo) or an `Int` where a
`String` is required, `orbit check` would reject the file with a located error
before anything is deployed.

The [quick start](getting-started/quickstart.md) runs this workflow end to
end.

## Guarantees

The runtime is event-sourced: every state change is an event committed to the
database before it takes effect, and the state of an execution is rebuilt from
its history after a restart. Tasks are delivered at least once, never
"exactly once". Each task carries an idempotency key that stays the same across
retries and redeliveries, so workers can make side effects idempotent. When the
outcome of an attempt is unknown, for example because a worker or the runtime
crashed mid-task, the runtime redelivers the same attempt and still accepts a
late result from the original delivery. Workflow definitions are immutable:
`name@version` is bound to a content hash, and running executions keep the
definition they started with. The compiler checks every step against its
contract, and the runtime validates execution inputs and worker outputs
against the same types. [Delivery guarantees](runtime/guarantees.md) gives the
precise rules.

## Current scope

Workflows can wait for typed signals, answer typed queries, compute values
without a worker, and run `on failure`, `on cancel` and `on timeout` handlers
after compensation; see [Signals and queries](language/signals.md),
[Compute steps](language/compute.md) and
[Outcome handlers](language/handlers.md). `orbit test` runs workflow tests
in-process with mocked tasks and simulated time, `orbit simulate` runs a
workflow locally with mocked or generated results, and `orbit replay` checks a
recorded history against the current decider; see
[Testing workflows](tooling/testing.md), [Simulation](tooling/simulate.md) and
[Replay](tooling/replay.md).

One server executes workflows at a time. For availability, several servers
can share one PostgreSQL database in active/standby mode: one leader runs the
engine, and a standby takes over if it stops. See
[Running several servers](runtime/cluster.md). The server can cap active
executions, fan-out, history length and payload sizes, and can give
applications client identities limited to some workflows; see
[Running the server](runtime/server.md#resource-limits) and
[Security](runtime/security.md#client-identities).

## Next steps

- [Installation](getting-started/installation.md): install the CLI, the
  runtime and an SDK.
- [Quick start](getting-started/quickstart.md): create a project and run a
  workflow with Python and TypeScript workers.
- [Core concepts](getting-started/concepts.md): the vocabulary used throughout
  this documentation.
- [Steps](language/steps.md): the full syntax of a step.
- [How workers work](sdks/overview.md): the worker model and the SDKs.
