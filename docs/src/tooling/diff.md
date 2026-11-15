# Process diffs

`orbit diff` compares two versions of a set of workflows and reports what the process does differently: steps added, removed or renamed, changed actions and contracts, dependencies, conditions, retries, timeouts, compensations, outcome and event handlers, signals, queries, input and output types, limits, and `continue_as_new`. It ends with the verdict of [`orbit compat`](compat.md), so one report answers both "what changed?" and "is it safe to deploy, and which version bump does it need?". The Markdown format is meant to be pasted into a pull request.

A text diff of the source shows edits; `orbit diff` compares the compiled definitions, so it ignores formatting and comments, sees dependencies implied by data references, and reports a step that was only renamed as a rename.

## Usage

```bash
orbit diff old/order.orbit new/order.orbit
orbit diff old/order.orbit new/order.orbit --format markdown > process-changes.md
orbit diff build/order.orbit.json workflows/order.orbit --workflow Order --exit-code
```

| Argument or option | Default | Meaning |
|---|---|---|
| `<OLD>`, `<NEW>` | required | A `.orbit` file (imports are followed) or a compiled IR `.json` file, for each side. |
| `--workflow <NAME>` | all | Report only this workflow, and only the compatibility findings for it and for task contracts. |
| `--format <FORMAT>` | `text` | `text`, `markdown` or `json`. |
| `--exit-code` | off | Exit `1` when there is any change. |

Exit codes, as with `git diff`:

| Status | When |
|---|---|
| `0` | Success. With `--exit-code`: no changes. |
| `1` | With `--exit-code`: there are changes. |
| `2` | A file cannot be read, is not valid IR, or does not compile, or `--workflow` names a workflow that neither side defines. |

`orbit diff` does not fail on an incompatible change; use [`orbit compat`](compat.md) in CI for that, or read `compatibility.compatible` from the JSON output.

## Example

The second version of [`examples/approval.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/approval.orbit) below lowers the approval threshold from 1000 to 500, shortens the reviewer's deadline, adds a retry policy and a timeout to the charge, renames `notify` to `tell_customer`, and adds an `on failure` handler:

```console
$ orbit diff approval.orbit v2/approval.orbit
workflow OrderApproval 1.0.0 → 1.1.0
  ~ workflow: version: 1.0.0 → 1.1.0
  > step: notify: notify → tell_customer
  ~ step input: assess: { needs_approval: input.amount >= 1000.0, rounded: float(int(input.amount * 100.0)) / 100.0 } → { needs_approval: input.amount >= 500.0, rounded: float(int(input.amount * 100.0)) / 100.0 }
  ~ timeout: decision: timeout 2d → timeout 1d
  ~ retry: charge: none → 3 attempts, exponential 1s..1m
  + timeout: charge: timeout 30s
  + handler: on failure: steps 1 (alert)
summary: workflows: 1 changed; 7 change(s): 2 added, 4 changed, 1 renamed; compatibility: compatible
```

Each line is `<sigil> <area>: <subject>: <before> → <after>`, with `+` added, `-` removed, `~` changed and `>` renamed. On a terminal the lines are coloured (disable with `NO_COLOR=1`).

### Markdown for a pull request

```console
$ orbit diff approval.orbit v2/approval.orbit --format markdown
```

```markdown
## Process changes

**Summary:** workflows: 1 changed; 7 change(s): 2 added, 4 changed, 1 renamed; compatibility: compatible

### Workflow `OrderApproval 1.0.0 → 1.1.0`

| Change | Area | Subject | Before | After |
|---|---|---|---|---|
| changed | workflow | `version` | `1.0.0` | `1.1.0` |
| renamed | step | `notify` | `notify` | `tell_customer` |
| changed | step input | `assess` | `{ needs_approval: input.amount >= 1000.0, rounded: float(int(input.amount * 100.0)) / 100.0 }` | `{ needs_approval: input.amount >= 500.0, rounded: float(int(input.amount * 100.0)) / 100.0 }` |
| changed | timeout | `decision` | `timeout 2d` | `timeout 1d` |
| changed | retry | `charge` | `none` | `3 attempts, exponential 1s..1m` |
| added | timeout | `charge` |  | `timeout 30s` |
| added | handler | `on failure` |  | `steps 1 (alert)` |

### Compatibility

**compatible**
```

### A change that needs a version bump

Adding a required input field and changing a timeout without changing the version:

```console
$ orbit diff approval.orbit v3/approval.orbit --exit-code
workflow OrderApproval 1.0.0
  + input: input.channel: String
  ~ timeout: decision: timeout 2d → timeout 1d
compatibility
  error   workflow OrderApproval@1.0.0: content changed without a version change; the runtime rejects it with DEFINITION_CONFLICT; hint: bump the version (1.0.0 → 2.0.0)
summary: workflows: 1 changed; 2 change(s): 1 added, 1 changed; compatibility: incompatible, 1 error(s) (bump the version (1.0.0 → 2.0.0))
$ echo $?
1
```

The `compatibility` section lists the `error` and `warning` findings of `orbit compat`; `ok` and `info` findings are left out. The verdict is `compatible`, `compatible, <n> warning(s)` or `incompatible, <n> error(s) (<hints>)`.

## What is compared

Workflows are matched by name. A workflow only in the new version is reported as `added`, one only in the old version as `removed`.

| Area | Changes reported |
|---|---|
| `workflow` | `version`, `description`, workflow `timeout` |
| `input`, `output` | Fields added, removed or retyped, and changed defaults, by path (`input.customer.email`, `output.items[].sku`, `input.prices{}.amount`) |
| `result` | The `return` expression |
| `signal` | Signals added or removed; payload fields added, removed or retyped (`signal review.comment`) |
| `query` | Queries added or removed; changed expression; changed result type |
| `step` | Steps added, removed or renamed |
| `action` | What the step runs: task and contract version, `using` language, child workflow, `sleep` duration, awaited signal, `compute` |
| `step input` | The `with` expression of a task or child workflow step, or the expression of a `compute` step |
| `dependency` | Direct dependencies added or removed (`after x`), explicit or implied by data references |
| `condition` | `when` added, removed or changed |
| `fan-out` | `for` variable, list expression and `max_parallel` |
| `retry` | Attempts, backoff, delays and non-retryable codes |
| `timeout` | `timeout`, `queue_timeout`, and the timeout of an `await` step |
| `idempotency` | `idempotency_key` added, removed or changed |
| `compensation` | Compensation added or removed, or a change of its task, contract version, input or retry policy |
| `handler` | `on failure`, `on cancel`, `on timeout` blocks added or removed. Steps of a block present in both versions are compared like main steps; their subject is `on <kind>: <step>`. |
| `event` | `on event <signal>` blocks, the same way |
| `limits` | The `limits` block |
| `continue_as_new` | [`continue_as_new`](../language/continue-as-new.md) added or removed (subject `continue_as_new`, value `when <condition> with <input>`, or `always with <input>`), or a change of its `condition` (`when ...` or `always`) or of its next `input` |
| `contract` | For a child workflow, a different child definition. Declared task contracts (also when no workflow uses them, and only without `--workflow`): added or removed contracts, version changes, `compensated_by` changes, and input and output field changes (`mail.send input.cc`) |

Expressions are compared as ORBIT source printed from the IR, so a change in formatting or comments is not a change.

### Renames

Steps are matched by scope (main graph, `on <kind>`, `on event <signal>`) and name. A step that exists only in the old version and a step that exists only in the new version, in the same scope, are reported as one rename when they run the same thing and are the only unmatched pair that does: the same task (any contract version), the same child workflow, an `await` of the same signal, or both `sleep` or both `compute`. The renamed step is then compared field by field, and dependencies on it are compared under its new name, so other steps that depend on it show no change. Expressions that mention the old name (for example a handler that reads `!is_null(charge)`) do change, and are reported.

If two removed steps call the same task, neither is treated as a rename; they are reported as removed and added.

### Continue-as-new

The second version of [`examples/subscription.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/subscription.orbit) below continues only once the receipt was delivered, and raises the amount by 2% each period:

```console
$ orbit diff subscription.orbit v2/subscription.orbit
workflow Subscription 1.0.0 → 1.1.0
  ~ workflow: version: 1.0.0 → 1.1.0
  ~ continue_as_new: condition: when input.period < input.periods → when input.period < input.periods && receipt.delivered
  ~ continue_as_new: input: { customer_id: input.customer_id, amount: input.amount, periods: input.periods, period: input.period + 1, charged: input.charged + input.amount } → { customer_id: input.customer_id, amount: input.amount * 1.02, periods: input.periods, period: input.period + 1, charged: input.charged + input.amount }
summary: workflows: 1 changed; 3 change(s): 3 changed; compatibility: compatible
```

The condition and the input are compared separately. Adding or removing `continue_as_new` is one change with the whole member as its value.

## JSON

`--format json` prints one pretty-printed object with the keys `summary`, `changes` (the number of changes), `workflows` (each with `name`, `old_version`, `new_version`, `status` and `changes`), `contracts` and `compatibility`:

```console
$ orbit diff approval.orbit v2/approval.orbit --format json | jq -c '{summary, changes, status: .workflows[0].status, rename: .workflows[0].changes[1], compatible: .compatibility.compatible}'
{"summary":"workflows: 1 changed; 7 change(s): 2 added, 4 changed, 1 renamed; compatibility: compatible","changes":7,"status":"changed","rename":{"kind":"renamed","area":"step","subject":"notify","old":"notify","new":"tell_customer"},"compatible":true}
```

`status` is `added`, `removed`, `changed` or `unchanged`; `kind` is `added`, `removed`, `changed` or `renamed`; `old` and `new` are strings or `null`. `compatibility` is the [`orbit compat --format json`](compat.md) report.

## In CI

Post the process changes as a pull request comment, for example with the GitHub CLI:

```bash
git show origin/main:workflows/order.orbit > /tmp/order.old.orbit
cp workflows/contracts.orbit /tmp/   # imports are resolved next to the file
orbit diff /tmp/order.old.orbit workflows/order.orbit --format markdown > diff.md
gh pr comment --body-file diff.md
```

## See also

- [Compatibility checks](compat.md)
- [Versioning](../language/versioning.md)
- [Workflow graphs](graph.md)
- [CLI: orbit diff](../reference/cli.md#orbit-diff)
