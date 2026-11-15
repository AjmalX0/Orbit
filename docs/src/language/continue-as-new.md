# Continue-as-new

An execution's history grows with every step it runs. A workflow that loops
for a long time, such as a subscription that bills every month or a poller
that fetches page after page, would record an unbounded history if it ran
inside one execution forever. `continue_as_new` ends the current run when its
steps are done and starts a new run of the same execution, with a new input
and an empty history. The execution keeps its id; each run is replayed, held
in memory and limited on its own. This page covers the syntax and why it has
this form, what happens at the switch, how runs are stored and read, and how
it combines with signals, children, limits and tests.

## Syntax

`continue_as_new` is a workflow member, next to `return`:

```orbit
continue_as_new [when <condition>] with <input>
```

When every step of the run has finished (the same point at which `return` is
evaluated), the runtime evaluates the condition. If it is `true`, or if there
is no `when`, the run ends and the next one starts with `<input>`; otherwise
the run completes with `return` as usual. `when` and `with` may come in either
order, and the expression after `with` may span several lines like any object
literal.

[`examples/subscription.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/subscription.orbit)
bills a customer once per period, with one run per period:

```orbit
import "contracts.orbit"

workflow Subscription {
    version "1.0.0"
    description "Charge a customer once per period, for a number of periods"
    input {
        customer_id: String
        amount: Float
        periods: Int = 12
        period: Int = 1
        charged: Float = 0.0
    }
    output { periods: Int, charged: Float }

    // The current period, readable at any time: `orbit query <id> period`.
    query period = { period: input.period, of: input.periods, charged: input.charged }

    step charge {
        worker "payments.charge"
        with {
            order_id: input.customer_id + "-" + string(input.period)
            amount: input.amount
            currency: "USD"
        }
        idempotency_key input.customer_id + "/period-" + string(input.period)
        retry { attempts: 5, backoff: exponential, initial_delay: 1m, max_delay: 1h }
    }

    step receipt {
        worker "notifications.send"
        after charge
        with {
            customer_id: input.customer_id
            message: "Payment " + charge.transaction_id + " for period " + string(input.period)
        }
    }

    step next_period { sleep 30d after receipt }

    // Not the last period: start the next one. `with` is typed against the
    // workflow input (omitted fields take their defaults).
    continue_as_new when input.period < input.periods with {
        customer_id: input.customer_id
        amount: input.amount
        periods: input.periods
        period: input.period + 1
        charged: input.charged + input.amount
    }

    return { periods: input.period, charged: input.charged + input.amount }
}
```

### Type checking

The input after `with` is checked against the workflow's own `input` type,
exactly like the input of a `worker` step against its contract: every required
field must be given with the right type, fields with defaults may be left out
(the defaults are applied when the run starts), and unknown fields are errors.
The expressions can read `input`, the outputs of the run's steps (a step that
may have been skipped reads as optional) and `execution`. The condition must
be a `Bool`; `null` counts as false.

| Code | Error |
|---|---|
| `E0323` | `continue_as_new` without `with <input>`. |
| `E0324` | The `when` condition is not a `Bool`. |
| `E0400`, `E0403` | The input does not match the workflow's `input` type. |
| `E0106` | A second `continue_as_new` in the workflow. |
| `W0607` | `continue_as_new` has no `when`, but the workflow has a `return`, which can then never be used. |

[Compiler diagnostics](../reference/diagnostics.md#e0323-continue_as_new-without-an-input)
shows the messages. A workflow with `continue_as_new` compiles to `orbit-ir/2`;
a workflow without it keeps its IR version and its definition hash
([Versioning](versioning.md#ir-version)).

### Why a declarative member

ORBIT workflows are declarations, not programs: steps and their dependencies
are a graph, and the only place a workflow decides what happens after its
steps is `return`. `continue_as_new` sits at that same place and has the same
shape as a step's `when` and `with`, so it reads like the rest of the language
and the compiler can check it completely: the next input is typed against the
workflow input, and both expressions see the same scope as `return`. Because
it is evaluated only when every step has finished, a run never switches with
work in flight: there are no running tasks, timers, children or event
handlers to carry over, only the input and the buffered signals. The
alternatives were a `return continue <expr>` form, which would make one
member return two kinds of things, and a dedicated step, which could run
while other steps were still in flight and would need rules for what happens
to them.

## What happens at the switch

When the decider finds the condition true, it records
`WorkflowContinuedAsNew { next_run, input }` as the run's terminal event (the
run's status is `CONTINUED_AS_NEW`). In the same database transaction, the
runtime:

1. appends that event to the finished run,
2. moves the finished run's history from `execution_events` to
   `execution_run_events`, keyed by execution id and run number,
3. writes the new run's first events: `WorkflowCreated` with `run`,
   `WorkflowStarted`, and one `SignalReceived` per buffered signal (below),
4. points the execution's row in `workflow_executions` at the new run.

A crash therefore leaves the execution either in the finished run (which is
decided again after recovery, with the same result) or in the new run; never
between them. The new run is then advanced like a newly started execution.

The `Inbox` workflow below takes one `message` signal per run and stops at
`stop`. Three signals were sent at once, while run 1 was sleeping:

```orbit
workflow Inbox {
    version "1.0.0"
    input { seen: Int = 0 }
    output { seen: Int }
    signal message { text: String }
    step take { await message timeout 1d }
    step pause { sleep 2s after take }
    continue_as_new when take.text != "stop" with { seen: input.seen + 1 }
    return { seen: input.seen + 1 }
}
```

```console
$ orbit logs inbox-1 --run 1
+     0ms   #1  WorkflowCreated
+     0ms   #2  WorkflowStarted
+     1ms   #3  SignalAwaitStarted        step=take signal=message
+    10ms   #4  SignalReceived            name=message
+    11ms   #5  SignalDelivered           step=take signal_seq=4
+    11ms   #6  TimerStarted              step=pause
+    20ms   #7  SignalReceived            name=message
+    28ms   #8  SignalReceived            name=message
+    2.1s   #9  TimerFired                step=pause
+    2.1s  #10  WorkflowContinuedAsNew    next_run=2
$ orbit logs inbox-1 --run 2
+     0ms   #1  WorkflowCreated           run=2
+     0ms   #2  WorkflowStarted
+     0ms   #3  SignalReceived            name=message
+     0ms   #4  SignalReceived            name=message
+     2ms   #5  SignalAwaitStarted        step=take signal=message
+     2ms   #6  SignalDelivered           step=take signal_seq=3
+     2ms   #7  TimerStarted              step=pause
+    2.1s   #8  TimerFired                step=pause
+    2.1s   #9  WorkflowContinuedAsNew    next_run=3
$ orbit logs inbox-1 --run 2 --json | head -4
{"seq":1,"timestamp_ms":1794769170400,"type":"WorkflowCreated","workflow":"Inbox","version":"1.0.0","definition_hash":"sha256:066240aa22c4be894c16cd8294f58c93a1d712db065c53d009825221a89cb028","input":{"seen":1},"run":2}
{"seq":2,"timestamp_ms":1794769170400,"type":"WorkflowStarted"}
{"seq":3,"timestamp_ms":1794769170400,"type":"SignalReceived","name":"message","payload":{"text":"second"}}
{"seq":4,"timestamp_ms":1794769170400,"type":"SignalReceived","name":"message","payload":{"text":"stop"}}
```

Run 1 took `first`; `second` and `stop` were still buffered when it ended, so
run 2 starts with both, in arrival order, and its `await` takes `second` at
once. Run 3 starts with `stop` and completes:

```console
$ orbit status inbox-1
COMPLETED inbox-1  Inbox@1.0.0
  run: 3 (continue-as-new)
  elapsed: 2.1s

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  take                         SUCCEEDED          1      0ms
  pause                        SUCCEEDED          1     2.1s

  output: {"seen":3}
```

## Runs

| Question | Answer |
|---|---|
| What does `GET /v1/executions/{id}` (and `orbit status`, `orbit inspect`) return? | The latest run's state. It has `run` (absent for run 1); `input`, `units`, `signals` and `created_at_ms` are the run's. |
| How do I read an earlier run? | `GET /v1/executions/{id}/history?run=N`, `orbit logs <id> --run N`, `orbit replay <id> --run N`. Each run's `seq` starts at 1. |
| What does `wait` wait for? | The final run: it returns when the latest run ends with a status other than `CONTINUED_AS_NEW`. |
| Where do signals go? | To the current run. Signals the finished run received but no step took are carried into the next run, in arrival order. A signal sent during the switch is recorded either before it (and carried over if nobody took it) or after it. |
| Queries? | They evaluate against the current run, and can read its `input`. |
| Cancellation? | `orbit cancel` cancels the current run, which then ends `CANCELLED`, and with it the execution. |
| The workflow `timeout`? | It applies to each run, from the run's start. |
| Which definition does the new run use? | The same `name@version` (and hash) as the finished run. To move a long-running execution to a new version, end it and start a new execution of the new version. |
| Child workflows? | A run's `workflow` steps start children with ids `<id>.<unit>` in run 1 and `<id>.run<N>.<unit>` in run N, so each run's children are new executions. A parent waiting for a child that continues as new waits for the child's final run and gets its output. |
| Idempotency keys? | The default key of a unit is `<id>/<unit>` in run 1 and `<id>/run<N>/<unit>` in run N. An explicit `idempotency_key` is used as written, so include something run-specific in it (the subscription uses the period number). |
| Does a new run count as a new execution? | No: it does not count against `--max-active-executions`, and `orbit_executions_started_total` does not change. `orbit_executions_continued_as_new_total` counts the switches. `GET /v1/executions` lists the execution once. |
| Shared mode? | The execution keeps its owner and ownership epoch. The finished run's task-queue rows are deleted in the same transaction. |

An execution started by a [schedule](../runtime/schedules.md) keeps its
`schedule_id` in every run, and the schedule's overlap policy sees it as
running until its final run ends.

## Combining it with history limits

`--max-history-events` (and a workflow's `limits { max_history_events: N }`)
apply to each run. A workflow that continues as new before a run reaches the
limit can run indefinitely under a small limit, and the limit then catches a
run that grows unexpectedly. The `Inbox` execution above recorded 27 events
across three runs on a server started with `--max-history-events 12`; no run
recorded more than 10.

```orbit
workflow Poller {
    version "1.0.0"
    input { cursor: String = "" }
    // A run that reaches 500 events fails with LIMIT_EXCEEDED; each run
    // here records about ten, then continues.
    limits { max_history_events: 500 }
    ...
    continue_as_new when page.has_more with { cursor: page.next }
}
```

Size a run so that it stays well below the limit: for a `for` step, each
element adds three to five events.

Continue-as-new bounds what one run holds and replays; the archived runs stay
in the database ([Storage: retention](../runtime/storage.md#retention)).

## Replay and tests

Each run replays on its own against the definition, with its carried signals
as external events:

```console
$ orbit replay inbox-1 --run 2
replayed 9 events of inbox-1 against Inbox@1.0.0 with the current decider
  5 decided, 4 external
consistent: every recorded decision is reproduced
```

`orbit test` and `orbit simulate` follow every run. Expectations are checked
against the final run, and `expect runs N` checks how many runs there were
([Testing workflows](../tooling/testing.md#expectations)):

```orbit
test "three periods are three runs" {
    workflow Subscription
    input { customer_id: "c-1", amount: 9.5, periods: 3 }
    mock "payments.charge" returns { transaction_id: "tx-" + input.order_id, status: "captured" }
    mock "notifications.send" returns { delivered: true }
    expect status COMPLETED
    expect runs 3
    expect output { periods: 3, charged: 28.5 }
}
```

## Limitations

- The switch happens only when every step has finished. There is no way to
  continue as new from inside a step, a handler or an `on event` block.
- A failure while evaluating the condition or the input (for example a
  division by zero) fails the run with `EXPRESSION_ERROR`; an input that does
  not match the workflow's `input` type at run time fails it with
  `INPUT_CONTRACT_VIOLATION`. Neither starts a new run.
- A workflow whose run finishes immediately (for example, only `compute`
  steps) and always continues starts a new run on every runtime tick. Give
  such a loop a `sleep` or an `await`.
- `GET /v1/executions/{id}` does not list the earlier runs; the latest run's
  `run` number tells how many there are.

## See also

- [Executions and history: runs](../runtime/executions.md#runs-continue-as-new)
- [Signals and queries](signals.md)
- [Running the server: resource limits](../runtime/server.md#resource-limits)
- [Schedules](../runtime/schedules.md)
- [HTTP API: history](../reference/http-api.md#get-v1executionsidhistory)
- [Testing workflows](../tooling/testing.md)
