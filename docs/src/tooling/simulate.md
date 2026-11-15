# Simulation

`orbit simulate` runs one execution of a workflow on your machine, without a server or workers. Tasks you do not describe get a generated result that matches their contract, so you can try a new workflow, explore a branch or watch a failure and its compensations as soon as the workflow type-checks. The simulator is the one [`orbit test`](testing.md) uses: the real engine and decider, an in-memory store, and a clock that jumps straight to the next timer, retry or timeout.

Use `orbit simulate` to explore; when a scenario is worth keeping, write it down as a [test](testing.md).

## Usage

```text
orbit simulate [OPTIONS] <FILE>
```

| Argument or option | Meaning |
|---|---|
| `<FILE>` | A `.orbit` file (imports are followed) or a compiled IR `.json` file. |
| `--workflow <WORKFLOW>` | Workflow to run when the file defines several. Without it, a file with one workflow, or with a parent and its children, runs the workflow no other workflow starts. |
| `-i`, `--input <JSON>` | The execution input. Default `{}`. It is validated like an input sent to the runtime, with defaults applied. |
| `--input-file <PATH>` | Read the input from a JSON file. |
| `--mocks <PATH>` | A JSON object mapping task names to the result each returns. |
| `--fail <TASK=CODE>` | Make every attempt of a task fail permanently with this error code. Repeatable. |
| `--signal <NAME=JSON>` | Send a signal once the execution waits for it, for example `review='{"approved":true,"reviewer":"dana"}'`. `NAME=@file.json` reads the payload from a file, and `NAME` alone sends `{}`. Repeatable; signals are sent in order. |
| `--json` | Print the result as JSON instead of the timeline. |

A task named in `--mocks` or `--fail` must be one the workflow can call; anything else is an error that lists the tasks. `--fail` takes precedence over `--mocks`.

Exit codes: `0` when the execution ends `COMPLETED`; `1` when it ends in any other status, cannot finish, or the file does not compile.

## Generated results

A task with neither a mock nor a `--fail` returns a sample built from its contract's output type. The rules are deterministic, so the same command always produces the same execution:

| Type | Sample |
|---|---|
| `String` | `"<field>-sample"`, using the field's name, for example `"transaction_id-sample"` |
| `Int` | `1` |
| `Float` | `1.0` |
| `Bool` | `true` |
| `Bytes` | `"c2FtcGxl"` (base64 of `sample`) |
| `Duration` | `1000` (one second) |
| `Timestamp` | `"2026-01-01T00:00:00Z"` |
| `UUID` | `"00000000-0000-4000-8000-000000000001"` |
| `JSON` | `{}` |
| `List<T>` | a list with one sample `T` |
| `Map<String, T>` | `{"key": <sample T>}` |
| `T?` | `null` |
| record | every field: its default if it has one, otherwise a sample of its type |

Samples follow the contract, not your business logic. In the order example, the generated risk score `1.0` is above the `0.8` threshold, so the payment step is skipped; give the task a realistic result with `--mocks` to take the other branch. The note on stderr lists which tasks got generated results.

Child workflows run as real child executions, and their tasks follow the same rules. The built-in `orbit.http.request` task is simulated like any other task, so a simulation never makes network requests.

## Examples

A large order with a reviewer's approval:

```console
$ orbit simulate examples/approval.orbit --input '{"order_id":"T1","customer_id":"c-1","amount":2500}' --signal 'review={"approved":true,"reviewer":"dana"}'
simulated OrderApproval@1.0.0 (0ms simulated, no server or workers)
note: generated sample results for: notifications.send, payments.charge

+     0ms   #1  WorkflowCreated
+     0ms   #2  WorkflowStarted
+     0ms   #3  StepSucceeded             step=assess attempt=1
+     0ms   #4  SignalAwaitStarted        step=decision signal=review
+     0ms   #5  SignalReceived            name=review
+     0ms   #6  SignalDelivered           step=decision signal_seq=5
+     0ms   #7  StepScheduled             step=charge attempt=1
+     0ms   #8  StepDispatched            step=charge attempt=1 worker_id=simulator-python
+     0ms   #9  StepSucceeded             step=charge attempt=1
+     0ms  #10  StepScheduled             step=notify attempt=1
+     0ms  #11  StepDispatched            step=notify attempt=1 worker_id=simulator-python
+     0ms  #12  StepSucceeded             step=notify attempt=1
+     0ms  #13  WorkflowCompleted

COMPLETED sim  OrderApproval@1.0.0
  elapsed: 0ms

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  assess                       SUCCEEDED          1
  charge                       SUCCEEDED          1      0ms  worker simulator-python
  decision                     SUCCEEDED          1      0ms
  notify                       SUCCEEDED          1      0ms  worker simulator-python

  output: {"status":"charged","transaction_id":"transaction_id-sample"}
```

A declined card in the order pipeline, with a realistic risk score from a mocks file:

```json
{ "risk.analyze": { "score": 0.3, "reasons": [] } }
```

```console
$ orbit simulate examples/order_processing.orbit --input '{"order_id":"A4","customer_id":"c-4","amount":75,"items":[{"sku":"MUG","quantity":2}]}' --mocks mocks.json --fail payments.charge=CARD_DECLINED
simulated OrderProcessing@1.0.0 (0ms simulated, no server or workers)
note: generated sample results for: inventory.release, inventory.reserve, orders.validate

+     0ms   #1  WorkflowCreated
+     0ms   #2  WorkflowStarted
+     0ms   #3  StepScheduled             step=validate_order attempt=1
+     0ms   #4  StepScheduled             step=analyze_risk attempt=1
+     0ms   #5  StepDispatched            step=analyze_risk attempt=1 worker_id=simulator-python
+     0ms   #6  StepDispatched            step=validate_order attempt=1 worker_id=simulator-typescript
+     0ms   #7  StepSucceeded             step=analyze_risk attempt=1
+     0ms   #8  StepSucceeded             step=validate_order attempt=1
+     0ms   #9  StepScheduled             step=reserve_inventory attempt=1
+     0ms  #10  StepDispatched            step=reserve_inventory attempt=1 worker_id=simulator-typescript
+     0ms  #11  StepSucceeded             step=reserve_inventory attempt=1
+     0ms  #12  StepScheduled             step=process_payment attempt=1
+     0ms  #13  StepDispatched            step=process_payment attempt=1 worker_id=simulator-python
+     0ms  #14  StepAttemptFailed         step=process_payment attempt=1 error=CARD_DECLINED simulated failure of `payments.charge` (--fail)
+     0ms  #15  StepFailed                step=process_payment error=CARD_DECLINED simulated failure of `payments.charge` (--fail)
+     0ms  #16  StepCancelled             step=send_notification
+     0ms  #17  CompensationStarted       reason=step `process_payment` failed: simulated failure of `payments.charge` (--fail)
+     0ms  #18  StepScheduled             step=compensate:reserve_inventory attempt=1
+     0ms  #19  StepDispatched            step=compensate:reserve_inventory attempt=1 worker_id=simulator-python
+     0ms  #20  StepSucceeded             step=compensate:reserve_inventory attempt=1
+     0ms  #21  CompensationCompleted
+     0ms  #22  WorkflowFailed            error=CARD_DECLINED step `process_payment` failed: simulated failure of `payments.charge` (--fail)

FAILED sim  OrderProcessing@1.0.0
  elapsed: 0ms
  compensation: completed
  error: CARD_DECLINED step `process_payment` failed: simulated failure of `payments.charge` (--fail)

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  analyze_risk                 SUCCEEDED          1      0ms  worker simulator-python
  compensate:reserve_inventory SUCCEEDED          1      0ms  worker simulator-python
  process_payment              FAILED             1      0ms  simulated failure of `payments.charge` (--fail)
  reserve_inventory            SUCCEEDED          1      0ms  worker simulator-typescript
  send_notification            CANCELLED          0
  validate_order               SUCCEEDED          1      0ms  worker simulator-typescript
```

Child executions are printed after the parent's timeline, under `child execution <id> (<workflow>):`.

## When a simulation cannot finish

The simulation stops, with a warning on stderr, when the execution cannot make progress: an `await` step without a timeout and no `--signal` left to send for it, or a step whose worker never answers. A signal the execution never waited for is reported as ``warning: signal `review` was not sent: the execution never waited for it``. In both cases the timeline and the state show where the execution stopped, and the exit code is `1`.

## JSON output

`--json` prints one object on stdout:

| Field | Meaning |
|---|---|
| `workflow`, `version` | The workflow that ran. |
| `status`, `output`, `error` | How the execution ended. |
| `simulated_ms` | Simulated time from start to finish. |
| `sampled_tasks` | Tasks that got generated results. |
| `problems`, `unsent_signals` | Why the simulation stopped early, and signals that were never sent. |
| `execution` | The final [execution state](../runtime/executions.md), as `GET /v1/executions/{id}` returns it. |
| `history` | Every history event, as `GET /v1/executions/{id}/history` returns them. |
| `children` | Child executions, each with `state` and `history`. |

The `history` array can be saved and replayed with [`orbit replay --history`](replay.md).

## See also

- [Testing workflows](testing.md)
- [Replay](replay.md)
- [CLI: orbit simulate](../reference/cli.md#orbit-simulate)
- [Executions and history](../runtime/executions.md)
- [Compensation](../language/compensation.md)
