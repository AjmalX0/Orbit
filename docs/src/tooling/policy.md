# Policies

A policy is a file of organisation rules that every workflow must follow: payments are compensated, large charges wait for an approval, every wait has a deadline, only approved services are called. `orbit check --policy <file>` checks the compiled workflows against it and reports violations as compiler diagnostics, located at the step or workflow that breaks the rule, with codes in the `P01xx` range. The same flag on `orbit compile` and `orbit deploy` refuses to produce or register a definition that violates an error rule.

Policies are checked on the compiled definitions, after type checking. They add no runtime behaviour: the runtime does not read policy files.

## Usage

```bash
orbit check   --policy policies/fintech.toml workflows/*.orbit
orbit compile --policy policies/fintech.toml workflows/*.orbit -o build
orbit deploy  --policy policies/fintech.toml workflows/*.orbit
orbit check   --policy policies/org.toml --policy policies/payments.toml workflows/*.orbit
```

`--policy` is repeatable; every policy is checked. It needs `.orbit` sources: `orbit deploy --policy` refuses compiled `.json` files, because violations and waivers are located in the source.

| Command | Policy errors | Policy warnings only |
|---|---|---|
| `orbit check` | diagnostics on stderr, `error: policy check failed: <e> error(s), <w> warning(s)`, exit `1` | diagnostics, `ok` lines, exit `0` |
| `orbit check --strict` | as above | exit `1`, like any warning |
| `orbit check --format json` | the `P` diagnostics are in `diagnostics`, exit `1` | exit `0` |
| `orbit compile` | writes nothing, exit `1` | writes the IR, exit `0` |
| `orbit deploy` | registers nothing from that file, exit `1`, before contacting the server | deploys, exit `0` |

Policies run only when the files compile without errors; compiler errors are reported as usual and policies are not checked. When every error is a policy error, the summary line is `policy check failed: ...`; otherwise it is `compilation failed: ...`.

## Example

[`examples/policies/fintech.toml`](https://github.com/AjmalX0/Orbit/blob/main/examples/policies/fintech.toml):

```toml
{{#include ../../../examples/policies/fintech.toml}}
```

[`examples/approval.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/approval.orbit) passes, with two warnings:

```console
$ orbit check --policy examples/policies/fintech.toml examples/approval.orbit
warning[P0106]: policy rule `failure-handler`: workflow `OrderApproval` has no `on failure` handler
  --> examples/approval.orbit:12:10
   |
12 | workflow OrderApproval {
   |          ^^^^^^^^^^^^^
   = note: policy fintech (examples/policies/fintech.toml): customers are told when their order fails
   = help: add `on failure { step ... }`, or waive it: `// policy-allow: failure-handler <reason>`

warning[P0104]: policy rule `payment-timeout`: step `charge` calls `payments.charge` without a `timeout`
  --> examples/approval.orbit:34:10
   |
34 |     step charge {
   |          ^^^^^^
   = note: required by policy fintech (examples/policies/fintech.toml)
   = help: add `timeout 1m` to the step, or waive it: `// policy-allow: payment-timeout <reason>`

ok OrderApproval@1.0.0 (4 steps)
```

Its charge is compensated, and it is guarded by the `review` approval as described under [`require_approval`](#require_approval). [`examples/checkout.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/checkout.orbit) charges without a compensation and without an approval:

```console
$ orbit check --policy examples/policies/fintech.toml examples/checkout.orbit
error[P0101]: policy rule `payments-compensated`: step `charge` calls `payments.charge` but has no compensation
  --> examples/checkout.orbit:22:10
   |
22 |     step charge {
   |          ^^^^^^
   = note: policy fintech (examples/policies/fintech.toml): every charge and reservation must be undone on failure
   = help: add a `compensate { worker "..." with { ... } }` block, or waive it: `// policy-allow: payments-compensated <reason>`

error[P0105]: policy rule `large-charge-approval`: step `charge` calls `payments.charge` without depending on an approval (`await` of `review`, `approval`) when input.amount >= 10000
  --> examples/checkout.orbit:22:10
   |
22 |     step charge {
   |          ^^^^^^
   = note: policy fintech (examples/policies/fintech.toml): charges of 10000 or more need a recorded approval
   = help: add a step that awaits the approval signal and make `charge` depend on it (`after` or by reading its output), or waive it: `// policy-allow: large-charge-approval <reason>`

warning[P0104]: policy rule `payment-timeout`: step `charge` calls `payments.charge` without a `timeout`
  --> examples/checkout.orbit:22:10
   |
22 |     step charge {
   |          ^^^^^^
   = note: required by policy fintech (examples/policies/fintech.toml)
   = help: add `timeout 1m` to the step, or waive it: `// policy-allow: payment-timeout <reason>`

error: policy check failed: 2 error(s), 1 warning(s)
$ echo $?
1
```

With `--format json`, policy diagnostics have the same shape as compiler diagnostics:

```console
$ orbit check --format json --policy examples/policies/fintech.toml examples/checkout.orbit | jq -c '.diagnostics[] | {severity, code, message, line: .location.line}'
{"severity":"error","code":"P0101","message":"policy rule `payments-compensated`: step `charge` calls `payments.charge` but has no compensation","line":22}
{"severity":"error","code":"P0105","message":"policy rule `large-charge-approval`: step `charge` calls `payments.charge` without depending on an approval (`await` of `review`, `approval`) when input.amount >= 10000","line":22}
{"severity":"warning","code":"P0104","message":"policy rule `payment-timeout`: step `charge` calls `payments.charge` without a `timeout`","line":22}
```

## File format

A policy is a TOML file with an optional `[policy]` table, any number of `[[rule]]` tables and any number of `[[exemption]]` tables. Unknown keys are errors, so a misspelt key cannot silently disable a rule.

```toml
[policy]
name = "fintech"                 # shown in diagnostics; default: the file name without extension
description = "..."              # optional
workflows = ["*"]                # default scope of every rule; default ["*"]
exclude_workflows = []           # default exclusions of every rule

[[rule]]
id = "payments-compensated"      # used in diagnostics and waivers; default: the type
type = "require_compensation"    # required, see the rule reference
severity = "error"               # "error" (default) or "warning"
description = "..."              # shown as a note on each violation
workflows = ["Checkout*"]        # this rule's scope; default: [policy] workflows
exclude_workflows = ["Legacy*"]  # default: [policy] exclude_workflows
tasks = ["payments.charge"]      # rule-specific keys
```

Rule ids must be unique within a file; give two rules of the same type their own `id`. An id is one word without spaces.

### Patterns

`workflows`, `exclude_workflows`, `tasks`, `signals` and `compensate_with` take glob patterns: `*` matches any run of characters, dots included, and `?` matches one character. `payments.*` matches `payments.charge` and `payments.eu.charge`; `*.charge` matches `payments.charge`; `*` matches everything. Matching is case-sensitive.

Task patterns match worker task names, including built-in tasks such as `orbit.http.request`. They never match child workflows (`workflow "Name"` steps).

### Errors in the policy file

A policy that cannot be loaded stops the command with exit `1` before anything is compiled or deployed, with a message naming the problem:

| Problem | Message (after `error: policy <file>:`) |
|---|---|
| TOML syntax, unknown key, wrong value type | the TOML parser's message with line and column |
| Unknown rule type | `rule 3: unknown type `...`; known types: ...` |
| Key that the rule type does not take | `rule `id`: `tasks` does not apply to `require_handler` (it takes: handlers)` |
| Missing required key | `rule `id`: `require_retry` needs `tasks = [...]`` |
| Invalid value | wrong `severity`, a `max` that is not a duration or a positive integer, `min_attempts` above `max_attempts`, an unknown handler or limit name, a malformed `when` |
| Duplicate `id` | `rule 2: duplicate id `...`` |
| Exemption for an unknown rule, or without a reason | `exemption 1: unknown rule `...``, `exemption 1 (...): `reason` is required` |

## Rules

| Type | Code | Checks | Keys |
|---|---|---|---|
| [`require_compensation`](#require_compensation) | `P0101` | Main steps calling matching tasks have a `compensate` block. | `tasks`, `compensate_with` |
| [`require_retry`](#require_retry-and-max_attempts) | `P0102` | Steps calling matching tasks retry: at least `min_attempts` attempts, at most `max_attempts`. | `tasks`, `min_attempts` (default 2), `max_attempts` |
| [`max_attempts`](#require_retry-and-max_attempts) | `P0103` | Steps calling matching tasks make at most `max` attempts. | `tasks` (default `["*"]`), `max` |
| [`require_timeout`](#require_timeout) | `P0104` | Steps calling matching tasks have a `timeout`, at most `max`; with `queue_timeout = true`, also a `queue_timeout`. | `tasks`, `max`, `queue_timeout` |
| [`require_approval`](#require_approval) | `P0105` | Steps calling matching tasks are guarded by an `await` of a matching signal, optionally only when a threshold is reached. | `tasks`, `signals`, `when`, `require_condition` (default true) |
| [`require_handler`](#require_handler) | `P0106` | Workflows declare the listed outcome handlers. | `handlers` (default `["failure"]`) |
| [`forbid_tasks`](#forbid_tasks-and-allow_tasks) | `P0107` | No step or compensation calls a matching task. | `tasks` |
| [`allow_tasks`](#forbid_tasks-and-allow_tasks) | `P0108` | Every step and compensation calls a matching task. | `tasks` |
| [`max_fan_out`](#max_fan_out) | `P0109` | Workflows with `for` steps declare `limits { max_fan_out }` of at most `max`; with `require_max_parallel = true`, every `for` step sets `max_parallel`. | `max`, `require_max_parallel` |
| [`require_limits`](#require_limits) | `P0110` | Workflows set the listed `limits`. | `keys` |
| [`require_signal_timeout`](#require_signal_timeout) | `P0111` | `await` steps of matching signals have a step `timeout`, at most `max`. | `signals` (default `["*"]`), `max`, `allow_workflow_timeout` |

Durations (`max` of `require_timeout` and `require_signal_timeout`) are strings in ORBIT syntax: `"500ms"`, `"30s"`, `"5m"`, `"2h"`, `"7d"`.

Unless stated otherwise, step rules apply to the main steps, the steps of `on failure`, `on cancel` and `on timeout`, and the steps of `on event` blocks. Rules that look at a task (`require_retry`, `max_attempts`, `require_timeout`, `require_approval`) look at what the step itself runs, not at its compensation.

### require_compensation

```toml
[[rule]]
type = "require_compensation"
tasks = ["payments.charge", "inventory.reserve"]
compensate_with = ["payments.refund", "inventory.release"]   # optional
```

Every main step that calls a matching task with `worker` has a `compensate` block; with `compensate_with`, the compensation calls a task matching one of those patterns. Handler and event steps are not checked, because the runtime only compensates main steps. This complements the compiler's own `compensated_by` check ([W0601](../language/safety.md#w0601-missing-compensation)): `compensated_by` is declared by the contract's owner, the policy by the organisation.

### require_retry and max_attempts

```toml
[[rule]]
type = "require_retry"
tasks = ["notifications.*"]
min_attempts = 3
max_attempts = 10

[[rule]]
id = "payment-retries"
type = "max_attempts"
tasks = ["payments.*"]
max = 3
```

The number of attempts is the step's `retry { attempts: n }`, including the first attempt; a step without `retry` makes one attempt.

### require_timeout

```toml
[[rule]]
type = "require_timeout"
tasks = ["*"]
max = "5m"
queue_timeout = true
```

The step must have `timeout` (the start-to-close limit of one attempt), not longer than `max`. With `queue_timeout = true` it must also have `queue_timeout`. A workflow `timeout` does not count.

### require_approval

```toml
[[rule]]
id = "large-charge-approval"
type = "require_approval"
tasks = ["payments.charge"]
signals = ["review", "approval"]
when = "input.amount >= 10000"   # optional
require_condition = true         # default
```

The rule asks: can a matching step run, when the threshold is reached, without a decision having been received from a matching signal and taken into account? The check is sound (when it passes, the property below holds) but conservative (some workflows that are safe are rejected); rewrite the conditions in the supported form, or waive the rule, when that happens.

`when` is optional and has the form `input.<field> <op> <number>`, with `<op>` one of `>=`, `>`, `<=`, `<`, `==`, `!=` and a nested field written `input.a.b`. Call it `P`; without `when`, `P` is "always". If the workflow's input has no numeric field at that path, the step is reported, because the check cannot show that the threshold is never reached.

A step `S` that calls a matching task passes if one of these holds:

1. **`S` cannot run when `P` holds.** `S` has a `when` condition that implies `not P`. For `when = "input.amount >= 10000"`, `when input.amount < 5000.0 && input.vip` passes.
2. **An approval guards `S`.** Some transitive dependency `A` of `S` in the same block (main steps, or the same `on` block) is an `await` of a signal matching `signals`, and:
   - `A` runs whenever `P` holds: `A` has no `when`, or `P` implies `A`'s `when`; and
   - unless `require_condition = false`, `S`'s `when` condition still reads `A`'s output after it is simplified under the assumption `P`. A step without `when` fails this part: it would run after the decision even if the reviewer rejected it.

What "implies" covers:

- A condition is split at `&&`, `||` and `!` (negations are pushed inward). The pieces that are understood are comparisons of an `input` field with a number literal, in either order (`input.amount >= 1000.0`, `1000 < input.amount`). Every other piece (calls, strings, step outputs, `if`) is unknown: it is never assumed true or false, so it can only make the check fail.
- Before the analysis, a reference to a field of a `compute` step in the same block that has no `when` and no `for` is replaced by the field's expression. In `examples/approval.orbit`, `assess.needs_approval` becomes `input.amount >= 1000.0`.
- Comparisons on the same field are compared as intervals of real numbers: `input.amount >= 10000` implies `input.amount >= 1000.0`, and implies `not (input.amount < 1000.0)`.
- "Simplified under `P`" replaces each understood comparison that `P` decides by `true` or `false` and simplifies. If the result is a constant, it does not read `A`.

`examples/approval.orbit` passes the fintech rule: the wait `decision` has `when assess.needs_approval`, which is `input.amount >= 1000.0`, and `input.amount >= 10000` implies it; the charge's `when !assess.needs_approval || coalesce(decision.approved, false)` becomes `input.amount < 1000.0 || coalesce(decision.approved, false)`, which under `P` simplifies to `coalesce(decision.approved, false)`, which reads `decision`.

A workflow whose wait only starts at 50000:

```orbit
task "payments.charge" {
    version "1.0.0"
    input { order_id: String, amount: Float }
    output { transaction_id: String }
}

workflow Payout {
    version "1.0.0"
    input { order_id: String, amount: Float }

    signal review { approved: Bool }

    step decision {
        await review
        when input.amount >= 50000.0
        timeout 2d
    }

    step charge {
        worker "payments.charge"
        after decision
        with { order_id: input.order_id, amount: input.amount }
    }
}
```

```console
$ orbit check --policy approval-policy.toml payout.orbit
error[P0105]: policy rule `large-charge-approval`: step `charge` calls `payments.charge` without a guaranteed approval when input.amount >= 10000
  --> payout.orbit:19:10
   |
19 |     step charge {
   |          ^^^^^^
   = note: `decision` may be skipped when input.amount >= 10000: its condition `input.amount >= 50000.0` is not implied by the rule
   = note: the `when` condition of `charge` does not read `decision` when input.amount >= 10000, so the decision does not gate the step
   = note: required by policy approval-policy (approval-policy.toml)
   = help: make `decision` run whenever the rule's condition holds (no `when`, or one the condition implies), and make `charge` read the decision, e.g. `when coalesce(decision.approved, false)`, or waive it: `// policy-allow: large-charge-approval <reason>`

error: policy check failed: 1 error(s), 0 warning(s)
```

(`approval-policy.toml` contains only the `large-charge-approval` rule shown above, with `signals = ["review"]`.) With `when input.amount >= 10000.0` on `decision` and `when input.amount < 10000.0 || coalesce(decision.approved, false)` on `charge` instead of `after decision`, the check passes:

```console
$ orbit check --policy approval-policy.toml payout2.orbit
ok Payout@1.0.0 (2 steps)
```

What the rule does not check: that the condition reads the right field of the decision (`decision.approved` rather than `decision.reviewer`), who sent the signal (use the runtime's [client permissions](../runtime/security.md) for that), or a decision taken inside a child workflow.

### require_handler

```toml
[[rule]]
type = "require_handler"
handlers = ["failure", "cancel"]
```

The workflow declares `on failure`, `on cancel` and/or `on timeout`, as listed. The violation is reported at the workflow name.

### forbid_tasks and allow_tasks

```toml
[[rule]]
id = "no-legacy"
type = "forbid_tasks"
tasks = ["legacy.*"]

[[rule]]
id = "known-services"
type = "allow_tasks"
tasks = ["payments.*", "inventory.*", "notifications.*"]
```

Both look at what every step runs and at what every compensation runs. `allow_tasks` also covers built-in tasks: add `orbit.*` to allow the [HTTP adapter](../language/http-adapter.md).

### max_fan_out

```toml
[[rule]]
type = "max_fan_out"
max = 500
require_max_parallel = true
```

A workflow with at least one `for` step must declare `limits { max_fan_out: n }` with `n <= max`, so that a large list fails the step with `LIMIT_EXCEEDED` instead of starting thousands of tasks. Workflows without `for` steps are not checked. With `require_max_parallel = true`, each `for` step must also set `max_parallel`.

### require_limits

```toml
[[rule]]
type = "require_limits"
keys = ["max_history_events", "max_payload_bytes"]
```

The workflow's `limits` block sets each listed key (`max_fan_out`, `max_history_events`, `max_payload_bytes`).

### require_signal_timeout

```toml
[[rule]]
type = "require_signal_timeout"
signals = ["review", "approval"]
max = "7d"
allow_workflow_timeout = false   # default
```

Each `await` of a matching signal has a step `timeout`, not longer than `max`. With `allow_workflow_timeout = true`, a workflow `timeout` (not longer than `max`) also satisfies the rule for steps without their own timeout. The compiler's [W0606](../language/safety.md#w0606-wait-without-a-timeout) accepts either timeout and has no maximum.

## Waivers and exemptions

A violation can be accepted in the workflow source, next to the step, or in the policy file.

### Inline waivers

A line comment `// policy-allow: <rule id> <reason>` in the comment block directly above a step waives that rule for the step. Above the `workflow` line, it waives the rule for the whole workflow. Several waivers can be stacked. The reason is required. When a step also has a [`///` doc comment](docs-generation.md#doc-comments), put the waiver above the doc comment: the doc comment must be directly above the step.

```orbit
    // policy-allow: payments-compensated the provider voids uncaptured authorisations after 7 days
    step charge {
        worker "payments.charge"
        after reserve
        with { order_id: input.order_id, amount: input.amount, currency: "USD" }
    }
```

Mistakes in waivers are diagnostics too. A copy of `examples/checkout.orbit` with three waivers, one valid, one without a reason, and one for a rule the step does not break:

```console
$ orbit check --policy policies/fintech.toml checkout.orbit
warning[P0002]: waiver for policy rule `payment-timeout` suppresses nothing
  --> checkout.orbit:13:5
   |
13 |     // policy-allow: payment-timeout reservations are local
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = help: remove the comment

error[P0001]: waiver for policy rule `large-charge-approval` has no reason
  --> checkout.orbit:24:5
   |
24 |     // policy-allow: large-charge-approval
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = help: write `// policy-allow: large-charge-approval <why this is acceptable>`

error[P0105]: policy rule `large-charge-approval`: step `charge` calls `payments.charge` without depending on an approval (`await` of `review`, `approval`) when input.amount >= 10000
  --> checkout.orbit:25:10
   |
25 |     step charge {
   |          ^^^^^^
   = note: policy fintech (policies/fintech.toml): charges of 10000 or more need a recorded approval
   = help: add a step that awaits the approval signal and make `charge` depend on it (`after` or by reading its output), or waive it: `// policy-allow: large-charge-approval <reason>`

warning[P0104]: policy rule `payment-timeout`: step `charge` calls `payments.charge` without a `timeout`
  --> checkout.orbit:25:10
   |
25 |     step charge {
   |          ^^^^^^
   = note: required by policy fintech (policies/fintech.toml)
   = help: add `timeout 1m` to the step, or waive it: `// policy-allow: payment-timeout <reason>`

error: policy check failed: 2 error(s), 2 warning(s)
```

| Code | Severity | Reported when |
|---|---|---|
| `P0001` | error | A waiver names a rule id that no `--policy` file defines (the note lists the ids), or has no reason. The waiver is ignored. |
| `P0002` | warning | A waiver suppresses no violation (the rule is not broken there, or the rule does not apply to this workflow). |

With several `--policy` files, a waiver may name a rule of any of them, and waives every rule with that id; a waiver that names no rule of any file is reported once. Waivers are comments, so adding or removing one does not change the definition hash.

### Exemptions in the policy file

```toml
[[exemption]]
rule = "large-charge-approval"
workflow = "Checkout"            # glob pattern
step = "charge"                  # optional: without it, the whole workflow
reason = "OPS-412: checkout is capped at 5000 by the storefront until the approval flow ships"
```

`rule` must be the id of a rule in the same file, and `reason` is required. `step` is a step name, or `on <kind>: <step>` / `on event <signal>: <step>` for handler and event steps. An exemption that matches no violation is reported as a warning (it does not change the exit status):

```console
$ orbit check --policy policies/exempt.toml checkout.orbit
warning: policy policies/exempt.toml: exemption 2 (rule `payment-timeout`, workflow `Legacy*`) matched no violation
warning[P0104]: policy rule `payment-timeout`: step `charge` calls `payments.charge` without a `timeout`
  --> checkout.orbit:23:10
   |
23 |     step charge {
   |          ^^^^^^
   = note: required by policy fintech (policies/exempt.toml)
   = help: add `timeout 1m` to the step, or waive it: `// policy-allow: payment-timeout <reason>`

ok Checkout@1.0.0 (2 steps)
```

Here `policies/exempt.toml` is `fintech.toml` with two exemptions appended (the one above and one for `payment-timeout` in `Legacy*` workflows), and `checkout.orbit` keeps only the `payments-compensated` waiver. Inline waivers are applied before exemptions; a violation covered by both counts as used by the waiver.

Use exemptions for decisions made by the policy's owners (with a ticket in the reason), and inline waivers for decisions that belong with the workflow and should be visible in its review.

## In CI

```yaml
- name: Check workflows against the organisation policy
  run: orbit check --strict --policy policies/fintech.toml workflows/*.orbit
```

`--strict` turns policy warnings into failures as well. To fail only on policy errors, leave it out.

## See also

- [Static safety checks](../language/safety.md): the compiler's built-in `W06xx` checks
- [Compiler diagnostics: policy](../reference/diagnostics.md#policy-p0xxx)
- [Process diffs](diff.md)
- [CLI: orbit check](../reference/cli.md#orbit-check)
