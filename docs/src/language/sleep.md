# Durable sleep

A `sleep` step waits for a fixed amount of time. The timer is kept by the
runtime in the execution history, so it survives runtime restarts and needs no
worker. This page covers the `sleep` action, the members it accepts, and how
timers behave when a workflow is aborted.

## Declaring a sleep step

```orbit
workflow FollowUp {
    version "1.0.0"
    input { email: String }

    step welcome {
        worker "emails.send"
        with { to: input.email, template: "welcome" }
    }

    step cool_off {
        sleep 24h
        after welcome
    }

    step follow_up {
        worker "emails.send"
        after cool_off
        with { to: input.email, template: "follow-up" }
    }
}
```

The duration is a duration literal: a number followed by `ms`, `s`, `m`,
`h` or `d`, for example `500ms`, `1.5h` or `7d`. It cannot be an expression,
so the wait time is fixed per workflow version. A bare number is `E0101`.

## Behavior

| Aspect | Behavior |
|--------|----------|
| Start | When the step becomes eligible, the runtime records `TimerStarted` with the wake-up time (now plus the duration). The unit is `WAITING`. |
| End | When the wake-up time passes, the runtime records `TimerFired` and the step succeeds. |
| Output | `{}`. Steps that only need ordering use `after`. |
| Restarts | Timers are recomputed from history. A timer that came due while the runtime was down fires right after startup. |
| Workers | None. Sleep steps are handled entirely by the runtime. |
| Workflow deadline | Sleeping counts against the workflow `timeout`. |
| Abort | If the workflow fails, is cancelled or times out while the timer is running, the step is withdrawn (`CANCELLED`). |
| Precision | Timers fire on the runtime's scheduling tick, so a timer may fire slightly after its wake-up time. |

`sleep 0s` is accepted and completes on the next tick.

## Allowed members

A sleep step accepts only `after` and `when`:

| Member | Allowed | Diagnostic |
|--------|---------|------------|
| `after` | yes | |
| `when` | yes | A skipped sleep step has output `null`. |
| `for` | no | `E0307` |
| `max_parallel` | no | `E0307` |
| `with`, `using`, `timeout`, `queue_timeout`, `retry`, `idempotency_key` | no | `E0307` |
| `compensate` | no | `E0308`: a sleep has nothing to compensate |

```orbit
workflow Wait {
    version "1.0.0"
    step pause {
        sleep 10m
        retry { attempts: 3 }
    }
}
```

```text
error[E0307]: `retry` is not allowed in a `sleep` step
 --> wait.orbit:5:9
  |
5 |         retry { attempts: 3 }
  |         ^^^^^^^^^^^^^^^^^^^^^

error: compilation failed: 1 error(s), 0 warning(s)
```

## Waiting until a point in time

ORBIT has no "sleep until" form, and expressions cannot read the clock,
so a sleep duration cannot be computed from a timestamp. To run work at a
specific time, start the execution at that time from an external scheduler.

## See also

- [Steps](steps.md)
- [Dependencies](dependencies.md)
- [Retries and timeouts](retries-timeouts.md#workflow-timeout)
- [Executions and history](../runtime/executions.md)
