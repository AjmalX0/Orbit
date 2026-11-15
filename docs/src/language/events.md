# Event handlers

An event handler is a block of steps that runs every time a given signal
arrives, alongside the workflow's main steps: `on event <signal> { ... }`.
Use it for messages that can come any number of times while an execution is
running, such as tracking scans, progress updates or comments. Use
[`await`](signals.md#waiting-for-a-signal-await) instead when the workflow
has to stop and wait for one specific message. This page covers the syntax,
what handler steps can read, how instances run, and the rules the compiler
enforces.

## Syntax

The complete example is [`examples/shipment.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/shipment.orbit), with a test in `examples/shipment.test.orbit` ([Testing workflows](../tooling/testing.md)).

```orbit
import "contracts.orbit"

workflow Shipment {
    version "1.0.0"
    input { shipment_id: String, customer_id: String }

    signal location { place: String, eta_hours: Int = 0 }
    signal delivered { signed_by: String }

    step arrival { await delivered timeout 30d }

    step confirm {
        worker "notifications.send"
        with {
            customer_id: input.customer_id
            message: "Shipment " + input.shipment_id + " was delivered, signed by " + arrival.signed_by
        }
    }

    on event location {
        step text {
            compute if event.eta_hours > 0
                then "Shipment " + input.shipment_id + " is in " + event.place + ", about " + string(event.eta_hours) + "h away"
                else "Shipment " + input.shipment_id + " is in " + event.place
        }
        step tell {
            worker "notifications.send"
            with { customer_id: input.customer_id, message: text }
        }
    }

    return { signed_by: arrival.signed_by }
}
```

The signal must be declared with `signal`. A workflow has at most one
`on event` block per signal, and a block contains `step` declarations and
nothing else. Send the signal as usual:

```bash
orbit signal ship-1 location --data '{"place":"Rotterdam","eta_hours":30}'
```

## What handler steps can read

| Name | Type | Notes |
|------|------|-------|
| `event` | the signal's payload type | The payload of the signal that started this instance, with defaults applied. |
| `input` | the workflow input | Unchanged. |
| `execution` | `{ id, workflow, version }` | As everywhere else. |
| a workflow step, e.g. `arrival` | that step's type, made optional | Main steps may not have run yet when a signal arrives, so they read as `null` until they succeed. Use `is_null` and `coalesce`. |
| an earlier step of the same block | its output type | Refers to the step of the same instance. Optional if that step has a `when`. Reading it adds a dependency. |

`event` exists only inside `on event` blocks; using it elsewhere is error
`E0401`.

## How instances run

Each arriving signal starts a new **instance** of the block. Instances run
concurrently with the main steps and with each other, and each has its own
copy of every step:

```text
+     0ms   #3  SignalAwaitStarted        step=arrival signal=delivered
+     0ms   #4  SignalReceived            name=location
+     0ms   #5  SignalDelivered           step=on_event:location@4 signal_seq=4
+     0ms   #6  StepSucceeded             step=on_event:location@4:text attempt=1
+     0ms   #7  StepScheduled             step=on_event:location@4:tell attempt=1
+     0ms   #8  StepDispatched            step=on_event:location@4:tell attempt=1 worker_id=simulator
+     0ms   #9  StepSucceeded             step=on_event:location@4:tell attempt=1
+     0ms  #10  SignalReceived            name=location
+     0ms  #11  SignalDelivered           step=on_event:location@10 signal_seq=10
+     0ms  #12  StepSucceeded             step=on_event:location@10:text attempt=1
...
+     0ms  #16  SignalReceived            name=delivered
+     0ms  #17  SignalDelivered           step=arrival signal_seq=16
+     0ms  #20  StepSucceeded             step=confirm attempt=1
+     0ms  #21  WorkflowCompleted
```

1. `SignalReceived` records the signal; `SignalDelivered` starts an instance.
   The instance key is `on_event:<signal>@<seq>`, where `<seq>` is the
   sequence number of the `SignalReceived` event, so keys are stable across
   restarts and replays. The instance unit's output is the payload.
2. The block's steps run in dependency order with unit keys
   `on_event:<signal>@<seq>:<step>`, for example `on_event:location@4:tell`.
   They retry, time out and dispatch to workers like any other step.
3. The execution completes only when the main steps **and every started
   instance** are finished. A signal that arrives while instances are still
   running starts another one.

`orbit status` lists instance units with the other steps, and `orbit inspect`
shows the block under `on event <signal>`.

## Failures and shutdown

- A step of an instance that fails permanently **fails the execution**, like
  a failing main step: steps that have not started (in every instance and in
  the main workflow) are cancelled, in-flight attempts finish, succeeded
  steps with a `compensate` are compensated, and the `on failure` handler
  runs.
- When the execution is cancelled, times out or fails, signals that arrive
  afterwards start no instance. A signal sent to a terminal execution is
  refused, as for any signal.
- A step with a `when` that is false is skipped, and steps that read it see
  `null`.

## Testing

`orbit test` sends a signal that an `on event` block handles as soon as the
execution is idle, without waiting for an `await`. In expectations, write the
step as `on_event:<signal>:<step>`. The expectation must hold for every
instance of the block, and at least one instance must have started:

```orbit
test "every scan notifies the customer" {
    workflow Shipment
    input { shipment_id: "S-1", customer_id: "c-1" }
    mock "notifications.send" returns { delivered: true }
    signal location { place: "Rotterdam", eta_hours: 30 }
    signal location { place: "Utrecht" }
    signal delivered { signed_by: "J. de Vries" }
    expect status COMPLETED
    expect step on_event:location:tell SUCCEEDED
}
```

## Rules

| Rule | Diagnostic |
|------|------------|
| The signal must be declared | `E0315` |
| One `on event` block per signal | `E0319` |
| A signal handled by `on event` cannot also be awaited | `E0322` |
| Step names must differ from workflow step names, `event` and `failure` | `E0302` |
| `after` may only name steps of the same block | `E0303` |
| Steps run a `worker` task, a `compute` expression or a `sleep`; `await`, child workflows, `for` and `max_parallel` are not allowed | `E0307` |
| Steps cannot declare `compensate` | `E0308` |
| `event` is read only inside `on event` steps | `E0401` |

```text
error[E0322]: signal `note` is handled by `on event` and cannot also be awaited
 --> aw.orbit:4:20
  |
4 |     step a { await note timeout 1h }
  |                    ^^^^
5 |     on event note {
  |              ---- handled here
  = note: every signal handled by `on event` starts the handler; an `await` would compete for it
```

Workflows with `on event` blocks compile to `orbit-ir/2`; see
[Versioning](versioning.md).

## See also

- [Signals and queries](signals.md)
- [Outcome handlers](handlers.md)
- [Compensation](compensation.md)
- [Testing workflows](../tooling/testing.md)
- [Compiler diagnostics](../reference/diagnostics.md)
