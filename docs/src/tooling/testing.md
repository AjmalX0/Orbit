# Testing workflows

A `test` declaration describes one execution of a workflow: its input, what each task returns, which signals arrive, and what you expect to happen. `orbit test` runs every test in-process on the real ORBIT engine, with an in-memory store and a simulated clock, so you need no server, no workers and no waiting: a test of a two-day approval timeout finishes in milliseconds. Run tests locally while you change a workflow, and in CI before you deploy.

## A first test

The [approval example](https://github.com/AjmalX0/Orbit/blob/main/examples/approval.orbit) charges an order, but orders of 1000 or more first wait for a `review` signal. This test sends an approval and checks the result:

```orbit
import "approval.orbit"

test "large orders wait for approval" {
    workflow OrderApproval
    input { order_id: "T1", customer_id: "c-1", amount: 2500.0 }
    mock "payments.charge" returns { transaction_id: "tx-" + input.order_id, status: "captured" }
    mock "notifications.send" returns { delivered: true }
    signal review { approved: true, reviewer: "dana" }
    expect status COMPLETED
    expect output { status: "charged", transaction_id: "tx-T1" }
    expect step decision SUCCEEDED
}
```

Keep tests in their own file that imports the workflow, by convention `<name>.test.orbit`. The repository's examples include [`approval.test.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/approval.test.orbit) and [`order_processing.test.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/order_processing.test.orbit):

```console
$ orbit test examples/*.test.orbit
running 10 tests
ok      small orders are charged without review (0ms simulated)
ok      large orders wait for approval (0ms simulated)
ok      rejected orders are not charged (0ms simulated)
ok      an order nobody reviews times out after two days (2d 0h simulated)
ok      a failed notification refunds the charge (0ms simulated)
ok      a low-risk order is charged (0ms simulated)
ok      a risky order is held for review (0ms simulated)
ok      risk scoring is retried after temporary failures (3.0s simulated)
ok      a declined card releases the reserved inventory (0ms simulated)
ok      a payment provider that never answers exhausts the retries (2m 37s simulated)

test result: ok. 10 passed, 0 failed, 0 filtered out (0.10s)
```

The time in parentheses is simulated time: how long the execution would have taken with real timers, retries and timeouts.

## Test declarations

```text
test "<name>" {
    workflow <Name>
    version "<version>"
    input { ... }
    mock "<task>" <behaviour> [times <n> then <behaviour> ...]
    signal <name> { ... }
    expect <expectation>
}
```

| Member | Required | Meaning |
|---|---|---|
| `workflow <Name>` | yes | The workflow to run. It must be defined in, or imported into, the compilation (`E0700`). |
| `version "<v>"` | no | Fails to compile (`E0702`) unless the sources define this version, so a test pinned to `1.0.0` notices a version bump. |
| `input { ... }` | when the workflow has required input fields | The execution input. It is checked like an input sent to the runtime: required fields, field types and unknown fields are compile errors, and defaults are applied. |
| `mock "<task>" ...` | for every task the execution calls | What the simulated worker answers. See [Mocks](#mocks). |
| `signal <name> { ... }` | no, repeatable | A signal sent to the execution. See [Signals](#signals). |
| `expect ...` | no, repeatable | What must be true when the execution has finished. See [Expectations](#expectations). |

Test names are strings and must be unique across the compilation (`E0709`). Values in `input`, signal payloads and expectations are constants: they can use literals, operators and [built-in functions](../language/functions.md), but not `input`, step outputs or `execution`.

Tests are part of the program: `orbit check` and the [language server](editors.md) report their errors, and a file with a broken test cannot be compiled or deployed. They are never part of the compiled output, so `orbit compile`, `orbit deploy` and `orbit run` ignore them, and adding a test does not change any workflow's definition hash.

## Mocks

The test runner acts as the worker for every task. A `mock` gives the result for one task, by its contract name:

| Behaviour | Effect on an attempt |
|---|---|
| `returns <expression>` | The task succeeds with this value. Inside the expression, `input` is the task's input (the evaluated `with` block), and `execution.id`, `execution.workflow` and `execution.version` describe the calling execution. |
| `fails "<CODE>"` | The attempt fails with this error code and the message ``mocked failure of `<task>` ``. The failure is permanent (`retryable: false`), so the step fails without retrying. |
| `fails "<CODE>" "<message>"` | The same, with your message. |
| `fails "<CODE>" retryable` | A retryable failure: the step's `retry` policy applies. |
| `hangs` | The worker never answers. The attempt's `timeout` fails it with `TIMEOUT` (retryable), and the retry policy applies. |

The compiler checks every mock against the task's contract: the task must be one the workflow calls in a step, a compensation or a child workflow (`E0703`), and a `returns` expression must have the contract's output type, with `input` typed as the contract's input (`E0400`, `E0402`, `E0403`). Each task can be mocked once per test (`E0704`).

Chain behaviours with `times <n> then`. Counting is per unit: each step, each element of a `for` step and each compensation counts its own calls, and every attempt (including a redelivery) is one call. The last behaviour answers every remaining call.

```orbit
import "order_processing.orbit"

test "risk scoring is retried after temporary failures" {
    workflow OrderProcessing
    input { order_id: "A3", customer_id: "c-3", amount: 10.0, items: [] }
    mock "orders.validate" returns { valid: true }
    mock "risk.analyze" fails "TEMPORARY" retryable times 2 then returns { score: 0.2, reasons: [] }
    mock "inventory.reserve" returns { reservation_id: "r-A3" }
    mock "payments.charge" returns { transaction_id: "tx-A3", status: "captured" }
    mock "notifications.send" returns { delivered: true }
    expect status COMPLETED
    expect step analyze_risk attempts 3
}
```

A task the execution calls without a mock stops the test with a failure that names the task and step:

```text
  ✗ task `notifications.send` (step `notify`) is not mocked; add `mock "notifications.send" returns { ... }` to the test
```

Child workflows are not mocked: they run as real child executions, and you mock the tasks they call. The built-in task `orbit.http.request` is mocked like any other task, so tests never make network requests.

## Signals

`signal <name> { payload }` sends a signal declared by the workflow (`E0705`). The payload is checked against the signal's declaration, with defaults applied; `signal approve` without a payload sends `{}`.

Signals are sent in the order you write them, each one as soon as the execution has a step waiting for that signal. Until then the next signal waits, and later signals wait behind it. Two consequences follow:

- A test of an `await` timeout simply sends no signal: with nothing to send, the runner advances the clock to the step's deadline.
- A signal the execution never waits for fails the test with ``signal `review` was never sent: the execution finished without waiting for it``. Remove the signal, or fix the condition that skipped the `await` step.

## Expectations

Expectations are checked once the execution has finished, and every failed expectation is reported, not just the first.

| Expectation | Passes when |
|---|---|
| `expect status <STATUS>` | The execution ended `COMPLETED`, `FAILED`, `CANCELLED` or `TIMED_OUT` (`E0707` for anything else). |
| `expect output { ... }` | The workflow's output contains these values. |
| `expect error "<CODE>"` | The execution failed with this error code, for example `CARD_DECLINED` from a failed step or `SIGNAL_TIMEOUT` from an `await` timeout. |
| `expect step <name> <STATUS>` | The step's unit ended in this status: `SUCCEEDED`, `SKIPPED`, `FAILED`, `CANCELLED`, or any other [unit status](../runtime/executions.md). `PENDING` means the step never started. |
| `expect step <name> output { ... }` | The step's output contains these values. |
| `expect step <name> attempts <n>` | The step's last attempt was attempt `n` (`0` for a step that never ran). |
| `expect compensated <name>` | The step's compensation ran and succeeded. For a `for` step, every element's compensation must have succeeded. The step must have a `compensate` block (`E0708`). |

Unknown step names are compile errors with a suggestion (`E0706`). Expected outputs are type-checked against the workflow's `output` (or the step's output type), with every field optional:

- An object matches when every field it lists matches; fields you leave out are not compared, at any depth.
- Lists and other values must be equal. Numbers compare by value, so `1` matches `1.0`.
- A field written as `null` matches a missing or `null` field.

A test without any `expect` still fails if the execution cannot finish; the compiler warns that it checks nothing (`W0701`).

## Time

Durable time is simulated. The clock starts at `2026-01-01T00:00:00Z`, and whenever nothing can happen (no task to answer, no signal to send), the runner jumps the clock straight to the next moment something is due: a `sleep` timer, a retry's backoff, an attempt `timeout`, a `queue_timeout`, an `await` timeout or the workflow `timeout`. Retry delays, timeouts and the order of events are exactly what the runtime would produce; only the waiting is skipped.

```orbit
import "approval.orbit"

test "an order nobody reviews times out after two days" {
    workflow OrderApproval
    input { order_id: "T3", customer_id: "c-3", amount: 1000.0 }
    expect status FAILED
    expect error "SIGNAL_TIMEOUT"
    expect step decision FAILED
}
```

If nothing is due and the execution still has not finished, for example an `await` step without a timeout and no signal to send, or a `hangs` mock on a step without a timeout, the test fails with the reason, such as ``the execution cannot make progress: step `decision` waits for signal `review`, and no signal is left to send``.

## How tests run

Each test runs on a fresh `orbit_runtime::Engine` with an in-memory store. Every workflow in the compilation is registered, children first, and the runner registers one simulated worker per language that steps ask for with `using`, announcing every contract. The execution has the id `sim`; child executions get their usual derived ids, such as `sim.ship`. Because the engine, the decider and contract validation are the same code the server runs, a mock result that violates the task's output contract fails the attempt with `OUTPUT_CONTRACT_VIOLATION`, as a real worker's would.

Tests are deterministic: the same sources always produce the same events, in the same order, at the same simulated times.

## Failure reports

A failing test prints each failed expectation and the execution's timeline (the same view as [`orbit logs`](../reference/cli.md#orbit-logs)), followed by the timelines of child executions:

```console
$ orbit test orders.test.orbit
running 1 test
FAILED  declined cards are reported (0ms simulated)

failures:

---- declined cards are reported (orders.test.orbit:3, OrderApproval@1.0.0)
  ✗ expected status COMPLETED, got FAILED (CARD_DECLINED: step `charge` failed: insufficient funds)
  ✗ expected output {"status":"charged"}, but the workflow produced none (it ended FAILED (CARD_DECLINED: step `charge` failed: insufficient funds))

  timeline of execution `sim`:
    +     0ms   #1  WorkflowCreated
    +     0ms   #2  WorkflowStarted
    +     0ms   #3  StepSucceeded             step=assess attempt=1
    +     0ms   #4  StepSkipped               step=decision reason=`when` condition was false
    +     0ms   #5  StepScheduled             step=charge attempt=1
    +     0ms   #6  StepDispatched            step=charge attempt=1 worker_id=simulator-python
    +     0ms   #7  StepAttemptFailed         step=charge attempt=1 error=CARD_DECLINED insufficient funds
    +     0ms   #8  StepFailed                step=charge error=CARD_DECLINED insufficient funds
    +     0ms   #9  StepCancelled             step=notify
    +     0ms  #10  WorkflowFailed            error=CARD_DECLINED step `charge` failed: insufficient funds

test result: FAILED. 0 passed, 1 failed, 0 filtered out (0.00s)
```

## Running tests

```text
orbit test [OPTIONS] [FILES]...
```

| Option | Meaning |
|---|---|
| `[FILES]...` | Files to compile. Every `test` in them, and in the files they import, runs. |
| `--filter <TEXT>` | Run only tests whose name contains this text. The others count as filtered out. |
| `--format <FORMAT>` | `human` (default) or `json`. |

Exit codes: `0` when every selected test passed (including when there are none); `1` when a test failed or the files do not compile. Compile errors are printed like `orbit check` prints them, and no test runs.

### In CI

Run `orbit test` next to `orbit check`. This repository's CI runs:

```bash
orbit test examples/*.test.orbit
```

### JSON format

`--format json` prints one object on stdout:

| Field | Type | Meaning |
|---|---|---|
| `tests[].name` | string | The test name. |
| `tests[].workflow`, `tests[].version` | string | The workflow under test. |
| `tests[].file`, `tests[].line` | string, integer | Where the test is declared. |
| `tests[].status` | string | `passed` or `failed`. |
| `tests[].failures` | array of strings | One message per failed expectation, or why the test could not run. |
| `tests[].execution` | object | `status`, `output`, `error` (`code`, `message`, `retryable`) and `simulated_ms` of the execution. |
| `tests[].history` | array | Failed tests only: the execution's history events, in the [history format](../runtime/executions.md#history-events). |
| `passed`, `failed`, `filtered_out` | integer | Counts. |

When the files do not compile, the object is `{"diagnostics": [...], "tests": []}`, with diagnostics in the `orbit check --format json` format, and the exit code is `1`.

```console
$ orbit test --format json orders.test.orbit
{
  "tests": [
    {
      "name": "declined cards are reported",
      "workflow": "OrderApproval",
      "version": "1.0.0",
      "file": "orders.test.orbit",
      "line": 3,
      "status": "failed",
      "failures": [
        "expected status COMPLETED, got FAILED (CARD_DECLINED: step `charge` failed: insufficient funds)",
        "expected output {\"status\":\"charged\"}, but the workflow produced none (it ended FAILED (CARD_DECLINED: step `charge` failed: insufficient funds))"
      ],
      "execution": {
        "status": "FAILED",
        "output": null,
        "error": {
          "code": "CARD_DECLINED",
          "message": "step `charge` failed: insufficient funds",
          "retryable": false
        },
        "simulated_ms": 0
      },
      "history": [ ... ]
    }
  ],
  "passed": 0,
  "failed": 1,
  "filtered_out": 0
}
```

## See also

- [Simulation](simulate.md)
- [Replay](replay.md)
- [CLI: orbit test](../reference/cli.md#orbit-test)
- [Compiler diagnostics: tests](../reference/diagnostics.md#tests-e07xx-w07xx)
- [Signals and queries](../language/signals.md)
- [Retries and timeouts](../language/retries-timeouts.md)
- [Compensation](../language/compensation.md)
