# Signals and queries

Signals and queries connect a running execution to the outside world. A
*signal* is a typed message that a person or another system sends to an
execution, such as a reviewer's decision; an `await` step waits for it
durably. A *query* is a typed, read-only view of an execution's state that
anyone can evaluate at any time, such as "where does this order stand?". Both
are declared in the workflow, so the compiler checks every use of them, and
the runtime validates every signal payload before it is recorded. This page
covers declarations, `await` steps, buffering and ordering, timeouts, sending
and querying from the CLI, HTTP and the SDKs, typing rules, errors and history
events, and ends with a complete example.

## Overview

| Construct | Syntax | Purpose |
|---|---|---|
| Signal declaration | `signal review { approved: Bool, reviewer: String }` | A message type the workflow accepts. Its fields are the payload. |
| `await` step | `step decision { await review timeout 2d }` | Waits for the next `review` signal. The payload is the step's output. |
| Query declaration | `query status = { approved: decision.approved }` | An expression evaluated on demand against the current state. |

```orbit
workflow Approval {
    version "1.0.0"
    input { request_id: String }

    signal review { approved: Bool, reviewer: String, comment: String? }

    step decision {
        await review
        timeout 2d
    }

    query status = {
        decided: !is_null(decision)
        approved: decision.approved
    }

    return { approved: decision.approved, reviewer: decision.reviewer }
}
```

Sending a signal and evaluating a query need an execution id and the declared
name:

```console
$ orbit signal <execution-id> review --data '{"approved": true, "reviewer": "dana"}'
$ orbit query <execution-id> status
```

## Declaring a signal

```text
signal <name> { <fields> }
```

A signal declaration is a workflow member. `<name>` is an identifier, unique
among the workflow's signals (`E0317`). The body is a record, written exactly
like a workflow `input` block: fields with types, optional fields (`T?`) and
constant defaults. Field types can be schemas, lists and maps:

```orbit
schema Reviewer {
    name: String
    team: String = "operations"
}

workflow Exceptions {
    version "1.0.0"
    timeout 30d

    signal review {
        approved: Bool
        by: Reviewer
        comment: String?
        tags: List<String> = []
    }

    step decision { await review }

    return { approved: decision.approved, team: decision.by.team }
}
```

Signal names and step names are separate namespaces, and so are query names.
A workflow can declare any number of signals. A signal that no step awaits is
warning `W0605` (see [Static safety checks](safety.md#w0605-signal-declared-but-never-awaited)).

### How the runtime validates a payload

When a signal arrives, the runtime:

1. applies the declared defaults to missing fields, including nested schema
   fields,
2. checks the payload against the declared type and rejects it with
   `422 SIGNAL_PAYLOAD_INVALID` if a required field is missing or a value has
   the wrong type; the message names the path, for example
   `` payload for signal `review` is invalid at $.approved: expected Bool, found a string ``,
3. records the validated payload in the history.

Fields that the declaration does not list are accepted and kept in the
recorded payload. (Workflow inputs are stricter: an execution input with an
undeclared field is rejected.) An optional field that is not sent stays
absent from the payload; reading it gives `null`.

## Waiting for a signal: `await`

```text
step <name> { await <signal> [timeout DURATION] [after ...] [when ...] }
```

`await <signal>` is a step action, like `worker` or `sleep`. It names a signal
declared in the same workflow; an unknown name is `E0315`, with a suggestion
for a close misspelling. When the step becomes eligible, the runtime records
`SignalAwaitStarted` and the unit is `WAITING`. When a signal of that name is
available, the runtime records `SignalDelivered` and the step succeeds with the
signal's payload as its output.

The step's output type is the signal's record type, so later steps read the
payload fields directly:

```orbit
task "refunds.issue" {
    input { order_id: String, amount: Float, note: String }
    output { refund_id: String }
}

workflow RefundRequest {
    version "1.0.0"
    input { order_id: String, amount: Float }

    signal decision { approved: Bool, note: String = "" }

    step review {
        await decision
        timeout 3d
    }

    step refund {
        worker "refunds.issue"
        when review.approved
        with { order_id: input.order_id, amount: input.amount, note: review.note }
    }

    return { approved: review.approved, refund_id: refund.refund_id }
}
```

### Members of an `await` step

An `await` step needs no worker, so most members do not apply:

| Member | Allowed | Meaning |
|---|---|---|
| `timeout` | yes | The longest the step waits, from `SignalAwaitStarted`. See [Timeouts](#timeouts). |
| `after` | yes | Start waiting only after these steps. Signals that arrive earlier are buffered. |
| `when` | yes | Wait only if the condition is true; otherwise the step is `SKIPPED` and its output is `null`. |
| `for`, `max_parallel` | no (`E0307`) | An `await` step receives one signal. Use several steps to receive several signals. |
| `with`, `using`, `retry`, `queue_timeout`, `idempotency_key` | no (`E0307`) | There is no task to call. |
| `compensate` | no (`E0308`) | Receiving a signal has nothing to undo. |

```orbit
workflow Approvals {
    version "1.0.0"
    input { reviewers: List<String> }
    signal review { approved: Bool }
    step decision {
        await review
        timeout 2d
        retry { attempts: 3 }
        with { reviewer: "dana" }
    }
}
```

```text
error[E0307]: `retry` is not allowed in an `await` step
 --> approvals.orbit:8:9
  |
8 |         retry { attempts: 3 }
  |         ^^^^^^^^^^^^^^^^^^^^^

error[E0307]: `with` is not allowed in an `await` step
 --> approvals.orbit:9:14
  |
9 |         with { reviewer: "dana" }
  |              ^^^^^^^^^^^^^^^^^^^^

error: compilation failed: 2 error(s), 0 warning(s)
```

### Output type

| Step | Type of `name` |
|---|---|
| `await s` | the record type of signal `s` |
| `await s` with `when` | the record type, optional (`{ ... }?`) |

Reading a field of an optional record gives an optional value, so
`decision.approved` has type `Bool?` when `decision` has a `when` condition.
Use `coalesce(decision.approved, false)` where a `Bool` is required.

## Buffering and ordering

Signals are buffered per execution. The runtime accepts a signal at any time
while the execution is not terminal, whether or not a step is waiting for it:

- A signal that arrives before its `await` step starts is kept in the buffer.
  When the step starts waiting, it takes the buffered signal right away.
- Each `await` step takes the oldest signal of its name that no step has taken
  yet. Signals of one name are therefore delivered in the order they were
  received.
- Each signal is delivered to at most one step. Two steps that wait for the
  same signal at the same time never receive the same signal.
- A signal that no step takes stays in the buffer, undelivered. It is ignored
  when the execution finishes.

The buffer is part of the execution state, in the `signals` array of
`GET /v1/executions/{id}` and `orbit inspect <id>` (see
[Executions and history](../runtime/executions.md#the-signal-buffer)).

In this workflow, both signals are sent while the first step is still sleeping:

```orbit
workflow Batch {
    version "1.0.0"
    timeout 1h

    signal item { sku: String, quantity: Int = 1 }

    step pause { sleep 2s }
    step first { await item after pause }
    step second { await item after first }

    return { skus: [first.sku, second.sku], total: first.quantity + second.quantity }
}
```

```console
$ orbit run batch.orbit --id batch-1 --no-wait
$ orbit signal batch-1 item --data '{"sku": "A-1"}'
signal `item` delivered to batch-1 (history #4)
$ orbit signal batch-1 item --data '{"sku": "B-2", "quantity": 3}'
signal `item` delivered to batch-1 (history #5)
$ orbit status batch-1
COMPLETED batch-1  Batch@1.0.0
  elapsed: 2.0s

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  first                        SUCCEEDED          1      0ms  
  pause                        SUCCEEDED          1     2.0s  
  second                       SUCCEEDED          1      0ms  

  output: {"skus":["A-1","B-2"],"total":4}
$ orbit logs batch-1
+     0ms   #1  WorkflowCreated           
+     0ms   #2  WorkflowStarted           
+     1ms   #3  TimerStarted              step=pause
+    13ms   #4  SignalReceived            name=item
+    24ms   #5  SignalReceived            name=item
+    2.0s   #6  TimerFired                step=pause
+    2.0s   #7  SignalAwaitStarted        step=first signal=item
+    2.0s   #8  SignalDelivered           step=first signal_seq=4
+    2.0s   #9  SignalAwaitStarted        step=second signal=item
+    2.0s  #10  SignalDelivered           step=second signal_seq=5
+    2.0s  #11  WorkflowCompleted         
```

`first` received the first signal and `second` the second one. The default
`quantity: Int = 1` was applied to the first payload.

`orbit signal` reports success (`signal ... delivered to <id>`) as soon as the
runtime has recorded and buffered the signal, whether or not a step is waiting
for it. The `SignalDelivered` event shows when a step took it.

## Timeouts

`timeout` on an `await` step bounds the wait. If no signal of that name is
available when the timeout passes, the step fails with the non-retryable error
`SIGNAL_TIMEOUT`, and the execution aborts like after any other step failure:
work that has not started is withdrawn and succeeded steps are compensated,
newest first. See [Compensation](compensation.md).

```orbit
task "payments.charge" {
    input { order_id: String, amount: Float }
    output { transaction_id: String }
}

task "payments.refund" {
    input { transaction_id: String, amount: Float }
    output { refunded: Bool }
}

workflow ConfirmCharge {
    version "1.0.0"
    input { order_id: String, amount: Float }

    signal confirm { ok: Bool }

    step charge {
        worker "payments.charge"
        with { order_id: input.order_id, amount: input.amount }
        compensate {
            worker "payments.refund"
            with { transaction_id: charge.transaction_id, amount: input.amount }
        }
    }

    step confirmation {
        await confirm
        after charge
        timeout 2s
    }
}
```

With no `confirm` signal sent:

```console
$ orbit logs confirm-1
+     0ms   #1  WorkflowCreated           
+     0ms   #2  WorkflowStarted           
+     1ms   #3  StepScheduled             step=charge attempt=1
+     3ms   #4  StepDispatched            step=charge attempt=1 worker_id=orders-python
+    12ms   #5  StepSucceeded             step=charge attempt=1
+    14ms   #6  SignalAwaitStarted        step=confirmation signal=confirm
+    2.0s   #7  StepFailed                step=confirmation error=SIGNAL_TIMEOUT no `confirm` signal arrived before the step's timeout
+    2.0s   #8  CompensationStarted       reason=step `confirmation` failed: no `confirm` signal arrived before the step's timeout
+    2.0s   #9  StepScheduled             step=compensate:charge attempt=1
+    2.0s  #10  StepDispatched            step=compensate:charge attempt=1 worker_id=orders-python
+    2.0s  #11  StepSucceeded             step=compensate:charge attempt=1
+    2.0s  #12  CompensationCompleted     
+    2.0s  #13  WorkflowFailed            error=SIGNAL_TIMEOUT step `confirmation` failed: no `confirm` signal arrived before the step's timeout
```

The deadline is recorded in `SignalAwaitStarted` (`deadline_ms`) and in the
unit's `wake_at_ms`, so it survives runtime restarts. A deadline that passed
while the runtime was down is handled right after startup.

Without a step `timeout`, the step waits until a signal arrives, the workflow
`timeout` passes, or the execution is cancelled. An `await` step with neither a
step timeout nor a workflow timeout is warning `W0606`, because the execution
could wait forever.

## Cancellation and other aborts

An `await` step that is waiting when the execution is cancelled, times out, or
fails elsewhere is withdrawn: the runtime records `StepCancelled` and the unit
ends `CANCELLED`. Signals sent during an abort are still validated and
recorded, but no step takes them. Once the execution is terminal, sending a
signal fails with `409 EXECUTION_FINISHED`.

```console
$ orbit cancel order-A-103 --reason "order withdrawn"
cancellation requested; execution is CANCELLED
$ orbit logs order-A-103 | tail -5
+   312ms   #5  CancelRequested           reason=order withdrawn
+   313ms   #6  StepCancelled             step=decision
+   313ms   #7  StepCancelled             step=charge
+   313ms   #8  StepCancelled             step=notify
+   313ms   #9  WorkflowCancelled         
```

## Sending a signal

Signals are sent by name to one execution. Sending requires the admin
credential when authentication is enabled, like every other execution
endpoint.

### CLI

```text
orbit signal <EXECUTION_ID> <NAME> [--data <JSON> | --data @<file>]
```

`--data` is the payload as JSON (default `{}`); `@path` reads it from a file.
On success the command prints the history sequence number of the recorded
`SignalReceived` event:

```console
$ orbit signal order-A-100 review --data '{"approved": true, "reviewer": "dana"}'
signal `review` delivered to order-A-100 (history #5)
$ orbit signal order-A-100 review --data @review.json
```

See [CLI: orbit signal](../reference/cli.md#orbit-signal).

### HTTP

```text
POST /v1/executions/{id}/signals/{name}
content-type: application/json

{"approved": true, "reviewer": "dana"}
```

The body is the payload. A request without a body sends `{}`. Response `200`:

```json
{"execution_id": "order-A-100", "signal": "review", "accepted": true, "seq": 5}
```

See [HTTP API](../reference/http-api.md#post-v1executionsidsignalsname).

### Python and TypeScript

```python
import os

from orbit_sdk import Client

client = Client(token=os.environ["ORBIT_ADMIN_TOKEN"])
seq = client.signal("order-A-100", "review", {"approved": True, "reviewer": "dana"})
```

```ts
import { Client } from "orbit-lang";

const client = new Client({ token: process.env.ORBIT_ADMIN_TOKEN });
const seq = await client.signal("order-A-100", "review", { approved: true, reviewer: "dana" });
```

Both return the sequence number and raise (`ApiError`) on an error response.
See [Python](../sdks/python.md#client) and
[TypeScript](../sdks/typescript.md#client).

### Errors

| Status | Code | When |
|---|---|---|
| 404 | `NOT_FOUND` | The execution does not exist. |
| 404 | `UNKNOWN_SIGNAL` | The execution's workflow does not declare this signal. The message lists the declared ones: `` OrderApproval@1.0.0 has no signal `reveiw`; declared: review ``, or `OrderApproval@1.0.0 declares no signals`. |
| 409 | `EXECUTION_FINISHED` | The execution is already terminal: `execution is already COMPLETED`. Checked before the signal name. |
| 422 | `SIGNAL_PAYLOAD_INVALID` | The payload does not match the declaration. Nothing is recorded. |

Signals are checked against the definition the execution is pinned to, not
against the latest registered version of the workflow.

## Queries

```text
query <name> = <expression>
```

A query declaration is a workflow member. `<name>` is an identifier, unique
among the workflow's queries (`E0318`). The expression can read `input`,
`execution` and the output of any step:

```orbit
workflow Progress {
    version "1.0.0"
    input { items: List<String> }

    step count { compute len(input.items) }
    step pause { sleep 1h after count }

    query summary = {
        id: execution.id
        items: coalesce(count, 0)
        paused: !is_null(pause)
    }
}
```

### Typing rules

A query can be evaluated at any moment, so any step may not have run yet.
The compiler therefore types every step output as optional in a query: a step
whose output is `T` is `T?` there (and a step that is already optional stays
optional). The query's result type is the type of the expression under that
rule, and it is recorded in the compiled definition.

```orbit
workflow Progress {
    version "1.0.0"
    input { items: List<String> }
    step count { compute len(input.items) }
    query remaining = count + 1
}
```

```text
error[E0406]: operator `+` cannot be applied to `Int?` and `Int`
 --> progress.orbit:5:23
  |
5 |     query remaining = count + 1
  |                       ^^^^^^^^^
5 |     query remaining = count + 1
  |                       ----- `Int?`
5 |     query remaining = count + 1
  |                               - `Int`
  = help: one operand may be `null`; use `coalesce(value, default)` first

error: compilation failed: 1 error(s), 0 warning(s)
```

Use `coalesce` and `is_null` to handle steps that have not run. Field access on
an optional record is allowed and gives `null` when the record is `null`, so
`decision.approved` is `null` until `decision` succeeds.

Queries are not steps: they add no dependencies, cannot be read by steps or
`return`, and do not appear in the step graph.

### Evaluation

The runtime evaluates a query when it is asked, against the execution's
current state:

- `input` is the execution input, with defaults applied.
- A step that has succeeded reads as its output. A step that has not
  succeeded (pending, waiting, running, skipped, failed or cancelled) reads as
  `null`.
- `execution.id`, `execution.workflow` and `execution.version` are available
  as in other expressions.

Evaluating a query is read-only. It records nothing in the history and does
not change the execution. It works on running and on finished executions, so
you can still read a query after the execution completes. An expression error
at evaluation time, such as a division by zero, returns `422 QUERY_FAILED`
with the cause; nothing is recorded.

### Reading a query

```console
$ orbit query order-A-100 status
{
  "needs_approval": true,
  "approved": null,
  "reviewer": null,
  "charged": false
}
```

```text
GET /v1/executions/{id}/queries/{name}
```

```json
{"execution_id":"order-A-100","query":"status","value":{"needs_approval":true,"approved":true,"reviewer":"dana","charged":true}}
```

```python
state = client.query("order-A-100", "status")
```

```ts
const state = await client.query<{ approved: boolean | null }>("order-A-100", "status");
```

`orbit query` prints the value only. The SDK methods return the value.

| Status | Code | When |
|---|---|---|
| 404 | `NOT_FOUND` | The execution does not exist. |
| 404 | `UNKNOWN_QUERY` | The workflow does not declare this query: `` OrderApproval@1.0.0 has no query `stat`; declared: status ``, or `Batch@1.0.0 declares no queries`. |
| 422 | `QUERY_FAILED` | Evaluating the expression failed: `` evaluating query `<name>`: <cause> ``. |

## History events

| Event | Fields | Meaning |
|---|---|---|
| `SignalReceived` | `name`, `payload` | A signal was accepted, validated and buffered. `payload` has defaults applied. Its `seq` identifies the signal. |
| `SignalAwaitStarted` | `step`, `signal`, `deadline_ms`? | An `await` step started waiting. `deadline_ms` is set when the step has a `timeout`. |
| `SignalDelivered` | `step`, `signal_seq` | The step took the signal recorded at history position `signal_seq`. The step succeeded with its payload as output. |

A timed-out wait is recorded as `StepFailed` with `SIGNAL_TIMEOUT`, and a
withdrawn wait as `StepCancelled`. Queries record no events. See
[Executions and history](../runtime/executions.md#signal-events).

## Complete example

[`examples/approval.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/approval.orbit)
charges an order. Orders of 1000 or more wait for a reviewer's `review`
signal; the `status` query reports where an order stands. The `assess` step is
a [compute step](compute.md), evaluated by the runtime. It runs with the
example Python and TypeScript workers.

```orbit
{{#include ../../../examples/approval.orbit}}
```

`decision` has a `when` condition, so its output is optional: `charge` reads
`coalesce(decision.approved, false)`. When `charge` is skipped, `notify` still
runs (a skipped dependency counts as done), and `is_null(charge)` selects the
message.

Start the runtime and the example workers:

```console
$ orbit server --db orbit.db
$ PYTHONPATH=sdks/python python3 examples/workers/python/order_workers.py
$ node examples/workers/typescript/order_workers.ts
```

Start an order that needs approval, and look at it while it waits:

```console
$ orbit run examples/approval.orbit --id order-A-100 --input '{"order_id": "A-100", "customer_id": "c-7", "amount": 2500.0}' --no-wait
deployed OrderApproval@1.0.0 (sha256:939fd564b818)
started execution order-A-100
order-A-100
$ orbit status order-A-100
RUNNING order-A-100  OrderApproval@1.0.0
  elapsed: 1ms

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  assess                       SUCCEEDED          1           
  decision                     WAITING            1           
$ orbit query order-A-100 status
{
  "needs_approval": true,
  "approved": null,
  "reviewer": null,
  "charged": false
}
```

A payload with the wrong type is rejected and nothing is recorded:

```console
$ orbit signal order-A-100 review --data '{"approved": "yes", "reviewer": "dana"}'
error: SIGNAL_PAYLOAD_INVALID (422): payload for signal `review` is invalid at $.approved: expected Bool, found a string
```

Send the decision:

```console
$ orbit signal order-A-100 review --data '{"approved": true, "reviewer": "dana", "comment": "verified by phone"}'
signal `review` delivered to order-A-100 (history #5)
$ orbit status order-A-100
COMPLETED order-A-100  OrderApproval@1.0.0
  elapsed: 569ms

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  assess                       SUCCEEDED          1           
  decision                     SUCCEEDED          1    539ms  
  charge                       SUCCEEDED          1      9ms  worker orders-python
  notify                       SUCCEEDED          1      6ms  worker orders-typescript

  output: {"status":"charged","transaction_id":"tx-A-100"}
$ orbit query order-A-100 status
{
  "needs_approval": true,
  "approved": true,
  "reviewer": "dana",
  "charged": true
}
```

The history shows the wait, the signal and its delivery. `assess` is a compute
step, so it succeeds without being scheduled or dispatched:

```console
$ orbit logs order-A-100
+     0ms   #1  WorkflowCreated           
+     0ms   #2  WorkflowStarted           
+     1ms   #3  StepSucceeded             step=assess attempt=1
+     1ms   #4  SignalAwaitStarted        step=decision signal=review
+   538ms   #5  SignalReceived            name=review
+   540ms   #6  SignalDelivered           step=decision signal_seq=5
+   540ms   #7  StepScheduled             step=charge attempt=1
+   547ms   #8  StepDispatched            step=charge attempt=1 worker_id=orders-python
+   556ms   #9  StepSucceeded             step=charge attempt=1
+   557ms  #10  StepScheduled             step=notify attempt=1
+   562ms  #11  StepDispatched            step=notify attempt=1 worker_id=orders-typescript
+   568ms  #12  StepSucceeded             step=notify attempt=1
+   569ms  #13  WorkflowCompleted         
$ orbit logs order-A-100 --json | grep Signal
{"seq":4,"timestamp_ms":1794746714132,"type":"SignalAwaitStarted","step":"decision","signal":"review","deadline_ms":1794919514132}
{"seq":5,"timestamp_ms":1794746714669,"type":"SignalReceived","name":"review","payload":{"approved":true,"reviewer":"dana","comment":"verified by phone"}}
{"seq":6,"timestamp_ms":1794746714671,"type":"SignalDelivered","step":"decision","signal_seq":5}
```

A rejection skips the charge:

```console
$ orbit run OrderApproval --id order-A-101 --input '{"order_id": "A-101", "customer_id": "c-8", "amount": 4000.0}' --no-wait
$ orbit signal order-A-101 review --data '{"approved": false, "reviewer": "lee", "comment": "customer could not be reached"}'
signal `review` delivered to order-A-101 (history #5)
$ orbit status order-A-101
COMPLETED order-A-101  OrderApproval@1.0.0
  elapsed: 328ms

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  assess                       SUCCEEDED          1           
  charge                       SKIPPED            0           
  decision                     SUCCEEDED          1    315ms  
  notify                       SUCCEEDED          1      5ms  worker orders-typescript

  output: {"status":"rejected","transaction_id":null}
```

A small order needs no approval: `decision` is skipped and the order is
charged straight away:

```console
$ orbit run OrderApproval --id order-A-102 --input '{"order_id": "A-102", "customer_id": "c-9", "amount": 120.0}'
started execution order-A-102
  ✓ assess                         succeeded 
  ↷ decision                       skipped 
  ✓ charge                         succeeded 7ms
  ✓ notify                         succeeded 9ms

COMPLETED order-A-102  OrderApproval@1.0.0
  elapsed: 22ms
{
  "status": "charged",
  "transaction_id": "tx-A-102"
}
```

## Changing signals and queries

Signals and queries are part of the workflow definition and its hash. Adding,
removing or changing one requires a new workflow version, and executions keep
the signals and queries of the version they started with. Senders and readers
depend on them like callers depend on the input, so `orbit compat` reports a
removed signal or query, a signal payload change that would reject payloads
that used to be valid, and a query result change that readers may not expect.
See [Compatibility checks](../tooling/compat.md#signals-and-queries).

A workflow that declares a signal or a query compiles to `orbit-ir/2`. See
[Versioning](versioning.md#ir-version).

## See also

- [Steps](steps.md)
- [Compute steps](compute.md)
- [Static safety checks](safety.md)
- [Executions and history](../runtime/executions.md)
- [HTTP API](../reference/http-api.md)
- [CLI](../reference/cli.md)
