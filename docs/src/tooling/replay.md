# Replay

The runtime records every execution as an event history and rebuilds an execution's state by folding that history (see [Executions and history](../runtime/executions.md)). The decisions in it, which step to schedule, whether to retry, when to compensate, were made by the decider of the ORBIT version that ran the execution. `orbit replay` re-runs a recorded history through the decider of the `orbit` binary you are running and reports every recorded decision it would not make again.

Use it to:

- check that upgrading ORBIT does not change how running executions continue: replay a sample of recent histories with the new binary before you upgrade the server;
- check that an edited workflow source still makes the decisions an execution recorded, before you deploy it under the same version;
- inspect an execution as it was right after any event (`--at`), with every step's status and output and the signal buffer.

Replay is read-only. It never changes the runtime or the history.

## Usage

```text
orbit replay [OPTIONS] [EXECUTION_ID]
```

| Argument or option | Meaning |
|---|---|
| `[EXECUTION_ID]` | Fetch the execution's history (`GET /v1/executions/{id}/history`) and the definition it ran (`GET /v1/workflows/{name}/{version}`) from the runtime. |
| `--history <PATH>` | Replay a history file instead: the response of `GET /v1/executions/{id}/history` (`{"execution_id": ..., "events": [...]}`), or a bare array of events. |
| `--definition <PATH>` | The definition to replay against: a compiled IR `.json` file, or `.orbit` source that is compiled first. Required with `--history`. With an execution id, it replaces the runtime's copy. |
| `--workflow <NAME>` | The workflow to use when `--definition` defines several. Default: the workflow named in the history's `WorkflowCreated` event. |
| `--at <SEQ>` | Stop after event `SEQ` and print the reconstructed state. |
| `--run <RUN>` | With an execution id: replay this run ([continue-as-new](../language/continue-as-new.md#replay-and-tests)) instead of the latest one (`GET /v1/executions/{id}/history?run=N`). Each run replays on its own; signals carried into a run are external events. |
| `--json` | Print the report as JSON. |
| runtime connection options | `--server`, `--token`, `--ca-cert`, `--client-cert`, `--client-key`; see [CLI](../reference/cli.md#runtime-connection-options). |

Exit codes: `0` when every recorded decision is reproduced (up to `--at`, if given); `1` on a divergence, or when the history or definition cannot be loaded.

## How replay decides

Replay starts from an empty state and goes through the history in order. Each event is either applied as recorded or checked against the decider:

| Origin | Events | How replay treats them |
|---|---|---|
| External | `WorkflowCreated`, `WorkflowStarted`, `StepDispatched`, `StepAttemptUncertain`, `SignalReceived`, `CancelRequested`, `StaleResultIgnored` | Applied as recorded. They record something the decider cannot know: a start, a dispatch to a worker, a lost worker, a signal, a cancellation. |
| External or decided | `StepSucceeded`, `StepAttemptFailed` | Consistent if the decider produces the event, or if it is a worker's result for a task, child workflow or built-in step that is still waiting for one. A `compute` step's result and a finished `for` step are decided, so a recorded `StepSucceeded` with a different value for them is a divergence. |
| Decided | every other event: `StepScheduled`, `StepSkipped`, `StepFanOut`, `TimerStarted`, `TimerFired`, `RetryScheduled`, `StepFailed`, `StepCancelled`, `SignalAwaitStarted`, `SignalDelivered`, `CompensationStarted`, `WorkflowCompleted`, `WorkflowFailed` and the rest | Must be among the events `decide(state, definition, time)` returns at that point, where time is the event's timestamp. |

The runtime commits one batch of decisions at a time, and the decider returns the events of a batch together, so their order within a batch does not matter. Times computed from the clock (`fire_at_ms` of timers and retries, `deadline_ms` of waits) are compared with a tolerance equal to the time since the previous event, because the runtime reads its clock just before it commits.

After a divergence, replay applies the recorded event and continues, so the state keeps matching what the runtime did and later divergences are independent findings.

The report also notes when the history was recorded against a different definition than the one you replay against (the definition hash in `WorkflowCreated` differs). That is expected when you replay against edited source; every decision that changed is then reported as a divergence.

## Examples

The examples use this workflow:

```orbit
workflow Reminder {
    version "1.0.0"
    input { name: String }
    signal ack { ok: Bool }
    step greet { compute { text: "hello " + input.name } }
    step wait { sleep 1s  after greet }
    step confirm { await ack  after wait  timeout 1h }
    return { text: greet.text }
}
```

An execution that ran on a local server, and its history:

```console
$ orbit logs rem-1
+     0ms   #1  WorkflowCreated
+     0ms   #2  WorkflowStarted
+     1ms   #3  StepSucceeded             step=greet attempt=1
+     1ms   #4  TimerStarted              step=wait
+    1.0s   #5  TimerFired                step=wait
+    1.0s   #6  SignalAwaitStarted        step=confirm signal=ack
+    2.0s   #7  SignalReceived            name=ack
+    2.0s   #8  SignalDelivered           step=confirm signal_seq=7
+    2.0s   #9  WorkflowCompleted
$ orbit replay rem-1
replayed 9 events of rem-1 against Reminder@1.0.0 with the current decider
  6 decided, 3 external
consistent: every recorded decision is reproduced
```

### Time travel

`--at` prints the state as it was right after an event: the units with their status and attempt, every step output, the signal buffer, and what the decider would do next.

```console
$ orbit replay rem-1 --at 6
replayed 6 events of rem-1 against Reminder@1.0.0 with the current decider
  4 decided, 2 external

state after #6 SignalAwaitStarted (+1009ms):

RUNNING rem-1  Reminder@1.0.0
  elapsed: 1.0s

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  greet                        SUCCEEDED          1
  confirm                      WAITING            1
  wait                         SUCCEEDED          1     1.0s

  step outputs:
    greet: {"text":"hello Ada"}
    wait: {}
```

Values whose keys look sensitive are redacted, as in [`orbit logs`](../reference/cli.md#orbit-logs).

### A divergence

Replaying the same execution against a source where `sleep 1s` became `sleep 10s` reports the one decision that changed. The timer still fires where the history says it did, because replay continues from the recorded event:

```console
$ orbit replay rem-1 --definition reminder-v2.orbit
replayed 9 events of rem-1 against Reminder@1.0.0 with the current decider
  5 decided, 3 external, 1 divergent
  note: the history was recorded with definition sha256:a442698aa0a986027f90c9213419a8aa868f59fffa35b1d8a7807c42fd7f4410, not sha256:fcdfa74b0e068e4eb5c7381adbdf5e4e2d4ad7515c24c70c75fe1bf6a9cc5e72

divergence at #4: the runtime recorded
    {"type":"TimerStarted","step":"wait","fire_at_ms":1794753908621}
  but the current decider would record
    {"type":"TimerStarted","step":"wait","fire_at_ms":1794753917621}
```

### Offline

Save a history and replay it later, or on another machine, against a compiled definition or the source:

```bash
curl -s "$ORBIT_SERVER/v1/executions/rem-1/history" > rem-1.json
orbit compile reminder.orbit -o build
orbit replay --history rem-1.json --definition build/reminder.orbit.json
```

The `history` array printed by [`orbit simulate --json`](simulate.md#json-output) and `orbit test --format json` (for failed tests) can be replayed the same way.

## JSON report

| Field | Meaning |
|---|---|
| `execution_id` | The replayed execution. |
| `events`, `external`, `decided` | How many events were replayed, and how many of each origin. |
| `divergences[]` | `seq`, the `recorded` event, and `decider`: the events the current decider returns at that point instead. |
| `origins[]` | `seq` and `origin` (`external`, `decided` or `divergent`) of every replayed event. |
| `definition_mismatch` | Present when the history ran another definition: `[recorded hash, replayed hash]`. |
| `pending` | What the decider would do next after the last replayed event. |
| `state` | The reconstructed execution state after the last replayed event. |

```console
$ orbit replay --history rem-1.json --definition reminder.orbit --json
{
  "execution_id": "rem-1",
  "events": 9,
  "external": 3,
  "decided": 6,
  "divergences": [],
  "origins": [
    {
      "seq": 1,
      "origin": "external"
    },
    ...
  ],
  "pending": [],
  "state": { ... }
}
```

## See also

- [Executions and history](../runtime/executions.md)
- [Delivery guarantees](../runtime/guarantees.md)
- [Versioning](../language/versioning.md)
- [Simulation](simulate.md)
- [CLI: orbit replay](../reference/cli.md#orbit-replay)
