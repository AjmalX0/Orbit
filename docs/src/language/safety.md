# Static safety checks

Besides type errors, the compiler looks for workflows that compile but are
likely to misbehave in production: a payment that is never refunded when a
later step fails, a step that can never run, a signal nobody waits for, a wait
that can last forever. It reports them as warnings in the `W06xx` range. This
page describes each check with real compiler output, the `compensated_by`
contract member that the compensation checks rely on, how to make warnings
fail a build with `--strict`, and what the checks do and do not guarantee.

## Summary

| Code | Reported when | Typical fix |
|---|---|---|
| [`W0601`](#w0601-missing-compensation) | A step calls a task whose contract declares `compensated_by`, and the step has no `compensate` block. | Add the compensation. |
| [`W0602`](#w0602-compensation-uses-a-different-task) | The step's `compensate` block calls a different task than the contract names. | Call the named task, or fix the contract. |
| [`W0603`](#w0603-step-never-runs) | A step's `when` condition is a constant that is false or `null`. | Remove the step, or make the condition depend on data. |
| [`W0604`](#w0604-condition-always-true) | A step's `when` condition is a constant `true`. | Remove `when`. |
| [`W0605`](#w0605-signal-declared-but-never-awaited) | A workflow declares a signal that no step awaits. | Add an `await` step, or remove the declaration. |
| [`W0606`](#w0606-wait-without-a-timeout) | An `await` step has no `timeout` and the workflow has no `timeout`. | Add a step or workflow timeout. |

Warnings do not stop compilation: `orbit check` exits `0`, `orbit compile`
writes the IR, and `orbit deploy` registers it. Use
[`--strict`](#treating-warnings-as-errors) to make them fail.

## Declaring how a task is undone: `compensated_by`

A task contract can name the task that undoes it:

```orbit
task "payments.charge" {
    version "1.0.0"
    input { order_id: String, amount: Float }
    output { transaction_id: String }
    compensated_by "payments.refund"
}

task "payments.refund" {
    version "1.0.0"
    input { transaction_id: String }
    output { refunded: Bool }
}
```

`compensated_by` takes a task name string. It must name a task declared in
the compilation, including imported files; an unknown name is `E0316`, with a
suggestion for close misspellings. With `compensated_by "payments.refnud"` in
the file above:

```text
error[E0316]: `compensated_by` names unknown task `payments.refnud`
 --> payments.orbit:5:20
  |
5 |     compensated_by "payments.refnud"
  |                    ^^^^^^^^^^^^^^^^^
  = help: did you mean `payments.refund`?

error: compilation failed: 1 error(s), 0 warning(s)
```

The declaration belongs to the contract, so every workflow that calls the
task is checked against it, by whoever writes the workflow. `compensated_by`
is checked only by the compiler; the runtime never runs a compensation that
the step does not declare. It is recorded in the contract inside the compiled
definition, so adding or changing it changes the definition hash of every
workflow that calls the task (see [Versioning](versioning.md)). It does not
by itself make a workflow `orbit-ir/2`.

## The checks

The examples below import this file as `payments.orbit`:

```orbit
task "payments.charge" {
    version "1.0.0"
    input { order_id: String, amount: Float }
    output { transaction_id: String }
    compensated_by "payments.refund"
}

task "payments.refund" {
    version "1.0.0"
    input { transaction_id: String }
    output { refunded: Bool }
}

task "payments.void" {
    version "1.0.0"
    input { transaction_id: String }
    output { voided: Bool }
}
```

### W0601: missing compensation

A step calls a task whose contract declares `compensated_by`, but the step has
no `compensate` block. If a later step fails, the charge would stay in place.

```orbit
import "payments.orbit"

workflow Checkout {
    version "1.0.0"
    input { order_id: String, amount: Float }

    step charge {
        worker "payments.charge"
        with { order_id: input.order_id, amount: input.amount }
    }
}
```

```text
warning[W0601]: step `charge` calls `payments.charge` but has no compensation
 --> checkout.orbit:8:16
  |
8 |         worker "payments.charge"
  |                ^^^^^^^^^^^^^^^^^
  = note: `payments.charge` declares `compensated_by "payments.refund"`: if a later step fails, its effect would be left in place
  = help: add `compensate { worker "payments.refund" with { ... } }`

ok Checkout@1.0.0 (1 steps)
```

Add the compensation:

```orbit
import "payments.orbit"

workflow Checkout {
    version "1.0.0"
    input { order_id: String, amount: Float }

    step charge {
        worker "payments.charge"
        with { order_id: input.order_id, amount: input.amount }
        compensate {
            worker "payments.refund"
            with { transaction_id: charge.transaction_id }
        }
    }
}
```

The check applies to every step that calls the task with `worker`, including
`for` steps.

### W0602: compensation uses a different task

The step has a compensation, but it calls another task than the one the
contract names.

```orbit
import "payments.orbit"

workflow Checkout {
    version "1.0.0"
    input { order_id: String, amount: Float }

    step charge {
        worker "payments.charge"
        with { order_id: input.order_id, amount: input.amount }
        compensate {
            worker "payments.void"
            with { transaction_id: charge.transaction_id }
        }
    }
}
```

```text
warning[W0602]: step `charge` compensates `payments.charge` with `payments.void`, but the contract names `payments.refund`
  --> checkout.orbit:11:20
   |
11 |             worker "payments.void"
   |                    ^^^^^^^^^^^^^^^
   = help: use `worker "payments.refund"` in the compensation, or change the contract's `compensated_by`

ok Checkout@1.0.0 (1 steps)
```

### W0603: step never runs

A `when` condition that reads nothing (no `input`, no step output, no
`execution` field, no loop variable) has the same value in every execution.
The compiler evaluates it. If it is `false` or `null`, the step is skipped in
every execution:

```orbit
task "audit.record" {
    input { order_id: String }
    output { recorded: Bool }
}

workflow Flags {
    version "1.0.0"
    input { order_id: String }

    step legacy_audit {
        worker "audit.record"
        when 1 > 2
        with { order_id: input.order_id }
    }

    step audit {
        worker "audit.record"
        when true || false
        with { order_id: input.order_id }
    }
}
```

```text
warning[W0603]: step `legacy_audit` never runs: its `when` condition is always false
  --> flags.orbit:12:14
   |
12 |         when 1 > 2
   |              ^^^^^
   = help: remove the step, or make the condition depend on input or step results

warning[W0604]: the `when` condition of step `audit` is always true
  --> flags.orbit:18:14
   |
18 |         when true || false
   |              ^^^^^^^^^^^^^
   = help: remove `when`; it also makes the step's output optional for no reason

ok Flags@1.0.0 (2 steps)
```

Constant conditions usually come from a feature flag that was switched off by
editing the source, or from a condition that was simplified too far.

### W0604: condition always true

The same analysis, for a constant condition that is `true` (see the example
above). The step always runs, but its output type is optional because of the
`when`, so every reader has to handle a `null` that never happens. Remove the
`when`.

### W0605: signal declared but never awaited

A workflow declares a [signal](signals.md) that no step awaits. Senders can
still send it, and the runtime accepts and buffers it, but nothing ever reads
it.

```orbit
workflow Review {
    version "1.0.0"

    signal review { approved: Bool }
    signal escalate { reason: String }

    step decision { await review }

    return { approved: decision.approved }
}
```

```text
warning[W0605]: signal `escalate` is declared but no step awaits it
 --> review.orbit:5:12
  |
5 |     signal escalate { reason: String }
  |            ^^^^^^^^
  = help: add a step with `await escalate`, or remove the declaration

warning[W0606]: step `decision` waits for signal `review` with no timeout
 --> review.orbit:7:27
  |
7 |     step decision { await review }
  |                           ^^^^^^
  = help: add `timeout 7d` to the step (or a workflow `timeout`) so the execution cannot wait forever

ok Review@1.0.0 (1 steps)
```

### W0606: wait without a timeout

An `await` step without a `timeout`, in a workflow without a `timeout`, waits
until a signal arrives or someone cancels the execution. If the sender has a
bug or the person never answers, the execution stays `RUNNING` forever. The
example above reports it for `decision`. Either bound works:

```orbit
workflow Review {
    version "1.0.0"
    timeout 30d

    signal review { approved: Bool }

    step decision {
        await review
        timeout 7d
    }

    return { approved: decision.approved }
}
```

A step timeout fails the step with `SIGNAL_TIMEOUT`, which starts
compensation. A workflow timeout ends the execution `TIMED_OUT`. See
[Signals and queries](signals.md#timeouts).

## Treating warnings as errors

`orbit check --strict` and `orbit compile --strict` fail when there is any
warning, including the non-safety warnings `W0201` (version is not semantic)
and `W0301` (unknown worker language):

```console
$ orbit check --strict review.orbit
warning[W0605]: signal `escalate` is declared but no step awaits it
 --> review.orbit:5:12
  |
5 |     signal escalate { reason: String }
  |            ^^^^^^^^
  = help: add a step with `await escalate`, or remove the declaration

warning[W0606]: step `decision` waits for signal `review` with no timeout
 --> review.orbit:7:27
  |
7 |     step decision { await review }
  |                           ^^^^^^
  = help: add `timeout 7d` to the step (or a workflow `timeout`) so the execution cannot wait forever

error: 2 warning(s) treated as errors (--strict)
$ echo $?
1
```

| Command | Without `--strict` | With `--strict` and only warnings |
|---|---|---|
| `orbit check` | prints warnings and `ok` lines, exits `0` | prints warnings and `error: <n> warning(s) treated as errors (--strict)`, no `ok` lines, exits `1` |
| `orbit check --format json` | prints the diagnostics, exits `0` | prints the diagnostics, exits `1` |
| `orbit compile` | writes the IR, exits `0` | writes nothing, exits `1` |

Errors take precedence: a file with errors fails the same way with or without
`--strict`. `orbit deploy` and `orbit run` do not take `--strict`; run
`orbit check --strict` (or `orbit compile --strict`) before them.

### In CI

Run the strict check on every change, next to the
[compatibility check](../tooling/compat.md):

```yaml
- name: Check workflows
  run: orbit check --strict workflows/*.orbit
```

To annotate a pull request, use the JSON format and read the `diagnostics`
array; the exit code still reflects `--strict`:

```bash
orbit check --strict --format json workflows/*.orbit > diagnostics.json
```

## What the checks guarantee

The checks are deliberately simple, so that a warning is always worth reading.
They are not a proof that a workflow is correct.

| Check | What a clean result means | What it does not cover |
|---|---|---|
| `W0601`, `W0602` | Every step that calls a task with `compensated_by` directly, with `worker`, declares a compensation that calls the named task. | Tasks without `compensated_by`. Whether the compensation's input identifies the right effect (it is type-checked like any `with`), whether the worker really undoes it, or whether the compensation succeeds at runtime. Child workflows that call the task are checked inside the child. |
| `W0603`, `W0604` | No `when` condition is a constant. | Conditions that read data but can never be true (`input.amount < 0 && input.amount > 0`) or are always true (`input.flag \|\| true`). Constant conditions whose evaluation fails, such as `1 / 0 > 0`; those fail at runtime with `EXPRESSION_ERROR`. |
| `W0605` | Every declared signal has at least one `await` step. | Whether that step can actually run: an `await` step with a condition that is never true still counts. |
| `W0606` | Every `await` step has a step or a workflow timeout. | Steps that wait for a worker. Use `timeout` and `queue_timeout` on task steps; see [Retries and timeouts](retries-timeouts.md). |

The compiler's type checks remain the main guarantee: every step input,
condition, query and `return` expression is checked against the declared
types before anything is deployed.

## See also

- [Compensation](compensation.md)
- [Task contracts](tasks.md)
- [Signals and queries](signals.md)
- [Conditions](conditions.md)
- [Compiler diagnostics](../reference/diagnostics.md#static-safety-w06xx)
- [CLI: orbit check](../reference/cli.md#orbit-check)
