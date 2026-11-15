# Compatibility checks

`orbit compat` compares two versions of your workflows and task contracts and reports every change that would break callers, result consumers, signal senders, query readers or workers, before you deploy. Run it in CI against the version on your main branch to catch a forgotten version bump or an accidental breaking change in review instead of in production.

## Why versions need checking

The runtime identifies a workflow definition by `name@version` and never changes a registered definition:

- Registering different content under an existing `name@version` is rejected with `DEFINITION_CONFLICT`. You find out at deploy time, after the change has been merged.
- Running executions keep the exact definition they started with. A new version only affects executions started after it is registered, and callers that start a workflow by name get the highest registered version.
- Workers receive a task only for the exact contract version they announce, and every workflow that uses a contract pins its version.

So a new version is a promise to the people who start the workflow, read its results and implement its tasks. `orbit compat` checks that promise against [semantic versioning](https://semver.org): a change that breaks callers or consumers needs a new major version. See [Versioning](../language/versioning.md) for the runtime rules this builds on.

## Usage

```text
orbit compat [OPTIONS] <OLD> <NEW>
```

| Argument or option | Meaning |
|---|---|
| `<OLD>` | The old version: a `.orbit` file (imports are followed) or a compiled IR `.json` file. |
| `<NEW>` | The new version, in either form. You can mix forms, for example a deployed `.json` file against a source file. |
| `--format <FORMAT>` | `human` (default) prints one line per finding on stdout. `json` prints a single report object on stdout. |

Both sides are compiled with the normal compiler, so a file with a compile error stops the check. Workflows are matched by name, and contracts by name and version. Built-in tasks such as `orbit.http.request` and child-workflow contracts are not compared as contracts; a changed child workflow shows up as a changed definition of its parent.

```console
$ orbit compat v1.orbit minor.orbit
error   workflow Invoice 1.4.0 → 1.5.0: input.amount: Float → Int: incompatible type: expected `Int`, found `Float`; breaking for callers without a major version bump; hint: bump the major version (1.4.0 → 2.0.0)
error   workflow Invoice 1.4.0 → 1.5.0: input.currency: (added) → String: required field added without a default; breaking for callers without a major version bump; hint: bump the major version (1.4.0 → 2.0.0)
ok      workflow Invoice 1.4.0 → 1.5.0: output.total: Float → Int: narrowed; still accepted where the old type is expected
error: compatibility check failed: 2 error(s), 0 warning(s)
```

Each line has the same parts:

```text
<severity> <workflow|contract> <name> <old version> → <new version>: <field path>: <old type> → <new type>: <message>; hint: <hint>
```

The field path starts at `input`, `output`, `signal <name>` or `query <name>`. `[]` is a list item and `{}` is a map value, for example `input.lines[].sku`. A field that only exists on one side is shown as `(added)` or `(removed)`. When both versions are equal, the version is shown once as `name@version`.

| Severity | Meaning | Affects the exit code |
|---|---|---|
| `error` | The new version would be rejected by the runtime, or breaks callers, consumers or workers without the version change that announces it. | Yes |
| `warning` | Something to review: a removed workflow, a breaking contract change in a non-major version, or a version that is not `MAJOR.MINOR.PATCH` so the bump cannot be checked. | No |
| `ok` | A compatible change, or an unchanged workflow or contract. | No |
| `info` | An addition, a removal that needs no action, or a breaking change that is announced by a major version bump. | No |

## How types are compared

Every type comparison uses the compiler's own assignability rules (the ones that check `with` blocks and `return`), so a change is compatible exactly when the language would accept the old value where the new type is expected, or the other way round for outputs:

| Change | As an input | As an output |
|---|---|---|
| `Int` → `Float` | compatible (widening) | breaking |
| `Float` → `Int` | breaking (narrowing) | compatible |
| `T` → `T?` | compatible | breaking |
| `T?` → `T` | breaking | compatible |
| `String` → `Int`, or any other unrelated type | breaking | breaking |
| `T` → `JSON` | compatible | breaking |
| `JSON` → `T` | breaking | compatible |

Records, lists, maps and optionals are compared field by field and item by item, so each change is reported at its own path. Two rules go beyond plain assignability, because the runtime validates more strictly than the type checker:

- Workflow inputs are validated strictly: the runtime rejects an input with a field the workflow does not declare. Removing a workflow input field is therefore breaking for callers that still send it.
- `JSON` accepts any value, so narrowing from `JSON` to a concrete type is breaking, even though the type checker allows `JSON` in both directions.

## Workflow rules

The checks run for every workflow name that exists in both versions.

| Situation | Result |
|---|---|
| Same version, same definition hash | `ok … unchanged` |
| Same version, different definition hash | `error`: the runtime rejects it with `DEFINITION_CONFLICT`. The hint suggests the next version: major for a breaking schema change, minor for an additive one, patch otherwise. |
| New version, identical definition | `info` |
| New version, input and output unchanged | `ok` |
| New version, compatible input or output change | one `ok` line per change |
| New version, breaking change, major bump | one `info` line per change |
| New version, breaking change, no major bump | one `error` line per change, with a hint naming the next major version |
| New version, breaking change, a version is not `MAJOR.MINOR.PATCH` | `warning`: the bump cannot be checked |
| New version lower than the old one | `warning` |
| Workflow only in the old version | `warning`: running executions keep their definition, but new starts fail once it is no longer registered |
| Workflow only in the new version | `info` |

A major bump means a higher `MAJOR`. For `0.x` versions a higher `MINOR` also counts, following the semver convention that anything may change before `1.0.0`. Pre-release and build suffixes are accepted and ignored for the comparison, so `1.0.0-rc.1` → `2.0.0-rc.1` is a major bump.

### Changed content under the same version

Old and new definitions of `Invoice@1.4.0` differ only in a sleep duration:

```console
$ orbit compat v1.orbit same.orbit
error   workflow Invoice@1.4.0: content changed without a version change; the runtime rejects it with DEFINITION_CONFLICT; hint: bump the version (1.4.0 → 1.4.1)
error: compatibility check failed: 1 error(s), 0 warning(s)
```

Formatting and comments do not change the definition hash, so `orbit fmt` never triggers this error.

### Input changes

Input changes affect callers: a caller that starts an execution with an input that was valid for the old version must still succeed.

| Change to an input field | Result |
|---|---|
| Field added as optional (`T?`) or with a default | compatible |
| Field added as required, without a default | breaking |
| Field removed | breaking (inputs are validated strictly) |
| Default added to a required field | compatible |
| Default removed | breaking |
| Type widened (`Int` → `Float`, `T` → `T?`, `T` → `JSON`) | compatible |
| Type narrowed or changed (`Float` → `Int`, `T?` → `T`, `String` → `Int`) | breaking |

In the example above, `amount: Float` became `amount: Int` and a required `currency: String` was added in a minor version. The same change in `2.0.0` passes, and each breaking change is reported as `info`:

```console
$ orbit compat v1.orbit major.orbit
info    workflow Invoice 1.4.0 → 2.0.0: input.amount: Float → Int: incompatible type: expected `Int`, found `Float`; breaking for callers, allowed by the major version bump
info    workflow Invoice 1.4.0 → 2.0.0: input.currency: (added) → String: required field added without a default; breaking for callers, allowed by the major version bump
ok      workflow Invoice 1.4.0 → 2.0.0: output.total: Float → Int: narrowed; still accepted where the old type is expected
```

A compatible minor version:

```orbit
workflow Invoice {
    version "1.5.0"
    input { customer_id: String, amount: Float, note: String?, currency: String = "EUR" }
    step pause { sleep 1s }
    return { total: input.amount, currency: input.currency }
}
```

```console
$ orbit compat v1.orbit compatible.orbit
ok      workflow Invoice 1.4.0 → 1.5.0: input.note: (added) → String?: optional field added
ok      workflow Invoice 1.4.0 → 1.5.0: input.currency: (added) → String: field with a default added
ok      workflow Invoice 1.4.0 → 1.5.0: output.currency: (added) → String: field added
```

### Output changes

Output changes affect consumers of results: anything that reads a completed execution's output.

| Change to an output field | Result |
|---|---|
| Field added | compatible |
| Required field removed | breaking |
| Optional field removed | compatible (consumers already handle `null`) |
| Type narrowed (`Float` → `Int`, `T?` → `T`) | compatible |
| Type made optional (`T` → `T?`), widened or changed | breaking |

```console
$ orbit compat v1.orbit output.orbit
error   workflow Invoice 1.4.0 → 1.5.0: output.total: Float → Float?: may now be null but consumers expect `Float`; breaking for consumers of results without a major version bump; hint: bump the major version (1.4.0 → 2.0.0)
error: compatibility check failed: 1 error(s), 0 warning(s)
```

### 0.x versions

Before `1.0.0`, a breaking change needs a new minor version:

```console
$ orbit compat r1.orbit r2.orbit
error   workflow Reminder 0.3.1 → 0.3.2: input.locale: (added) → String: required field added without a default; breaking for callers without a major version bump; hint: bump the minor version (0.3.1 → 0.4.0); in 0.x versions the minor version is the major version
error: compatibility check failed: 1 error(s), 0 warning(s)
```

### Signals and queries

[Signals](../language/signals.md) are inputs that other systems send to running executions, and [queries](../language/signals.md#queries) are outputs they read, so `orbit compat` compares them like the workflow input and output. Signals and queries are matched by name; paths are `signal <name>` and `query <name>`, followed by the field path.

| Change | Result |
|---|---|
| Signal removed | breaking: senders get `UNKNOWN_SIGNAL` |
| Signal payload field added as optional (`T?`) or with a default | compatible |
| Signal payload field added as required, without a default | breaking |
| Signal payload field removed | compatible: the runtime accepts and keeps fields a signal does not declare, so old senders keep working |
| Signal payload field widened or narrowed | as for an input field: widening is compatible, narrowing or an unrelated type is breaking |
| Query removed | breaking: readers get `UNKNOWN_QUERY` |
| Query result changed | as for an output: a field added or a type narrowed is compatible; a field removed, a type made optional, widened or changed is breaking |
| Signal or query added | not reported |

A breaking change is an `error` without a major version bump and `info` with one, as for inputs and outputs. The message names who is affected: `breaking for senders of the signal` or `breaking for readers of the query`.

Old version:

```orbit
workflow Approval {
    version "1.0.0"
    timeout 7d

    signal review { approved: Bool, comment: String? }
    signal escalate { reason: String }

    step decision { await review }
    step escalation { await escalate after decision when !decision.approved }

    query state = { approved: decision.approved, comment: decision.comment }
    query escalated = !is_null(escalation)

    return { approved: decision.approved }
}
```

A minor version that drops the `escalate` signal and the `escalated` query, adds a required `reviewer` field to `review`, and makes `state.comment` non-optional:

```orbit
workflow Approval {
    version "1.1.0"
    timeout 7d

    signal review { approved: Bool, comment: String?, reviewer: String }

    step decision { await review }

    query state = { approved: decision.approved, comment: coalesce(decision.comment, "") }

    return { approved: decision.approved }
}
```

```console
$ orbit compat a1.orbit a2.orbit
error   workflow Approval 1.0.0 → 1.1.0: signal escalate: { reason: String } → (removed): signal removed: senders get UNKNOWN_SIGNAL; breaking for senders of the signal without a major version bump; hint: bump the major version (1.0.0 → 2.0.0)
error   workflow Approval 1.0.0 → 1.1.0: signal review.reviewer: (added) → String: required field added without a default; breaking for senders of the signal without a major version bump; hint: bump the major version (1.0.0 → 2.0.0)
error   workflow Approval 1.0.0 → 1.1.0: query escalated: Bool → (removed): query removed: readers get UNKNOWN_QUERY; breaking for readers of the query without a major version bump; hint: bump the major version (1.0.0 → 2.0.0)
ok      workflow Approval 1.0.0 → 1.1.0: query state.comment: String? → String: no longer null
error: compatibility check failed: 3 error(s), 0 warning(s)
```

The same changes as `2.0.0` are reported as `info` (`breaking for senders of the signal, allowed by the major version bump`). A compatible minor version keeps both signals and queries, gives the new `reviewer` field a default, and adds fields:

```orbit
workflow Approval {
    version "1.2.0"
    timeout 7d

    signal review { approved: Bool, comment: String?, reviewer: String = "unknown", priority: Int? }
    signal escalate { reason: String }

    step decision { await review }
    step escalation { await escalate after decision when !decision.approved }

    query state = { approved: decision.approved, comment: decision.comment, reviewer: decision.reviewer }
    query escalated = !is_null(escalation)

    return { approved: decision.approved }
}
```

```console
$ orbit compat a1.orbit a3.orbit
ok      workflow Approval 1.0.0 → 1.2.0: signal review.reviewer: (added) → String: field with a default added
ok      workflow Approval 1.0.0 → 1.2.0: signal review.priority: (added) → Int?: optional field added
ok      workflow Approval 1.0.0 → 1.2.0: query state.reviewer: (added) → String?: field added
```

Query result types follow the [query typing rules](../language/signals.md#typing-rules): every step output is optional in a query, so `decision.approved` has type `Bool?` there. In this pair, the new version (`1.1.0`) changes `review.approved` from `Bool` to `Int`, removes the optional `review.comment`, and returns `string(decision.approved)` in `state.approved`:

```console
$ orbit compat b1.orbit b2.orbit
error   workflow Approval 1.0.0 → 1.1.0: signal review.approved: Bool → Int: incompatible type: expected `Int`, found `Bool`; breaking for senders of the signal without a major version bump; hint: bump the major version (1.0.0 → 2.0.0)
error   workflow Approval 1.0.0 → 1.1.0: signal review.comment: String? → (removed): field removed; values that still contain it are rejected, because inputs are validated strictly; breaking for senders of the signal without a major version bump; hint: bump the major version (1.0.0 → 2.0.0)
error   workflow Approval 1.0.0 → 1.1.0: query state.approved: Bool? → String: incompatible type: expected `Bool`, found `String`; breaking for readers of the query without a major version bump; hint: bump the major version (1.0.0 → 2.0.0)
error: compatibility check failed: 3 error(s), 0 warning(s)
```

The runtime itself does not reject undeclared signal payload fields (it keeps them in the payload), so a removed payload field breaks senders only in the sense that the field is no longer read. `orbit compat` reports it as breaking all the same, so that removing a field is a deliberate, major change.

### Removed and added workflows

Renaming a workflow removes one name and adds another:

```console
$ orbit compat v1.orbit renamed.orbit
warning workflow Invoice@1.4.0: workflow removed; running executions keep their definition, but new starts fail once it is no longer registered
info    workflow Billing@1.0.0: workflow added
```

A removal is a warning, not an error: the registered definition stays in the runtime, and running executions finish on it. Check that no caller still starts the old name.

## Contract rules

A task contract has two sides. The workflow is the caller: it sends the input and reads the output. The worker is the implementer: it reads the input and returns the output. Because the runtime matches workers by exact `name@version`, the rules depend on whether the contract version changed.

### Same contract version

Under one contract version, old and new workers both announce `name@version`, and old and new workflows both use it, while executions on old workflow versions are still running. Any worker must be able to serve any of those workflows, so a change is allowed only if it is compatible in both directions:

- input: the old input type must be assignable to the new one, and the new to the old;
- output: the old output type must be assignable to the new one, and the new to the old.

Contract inputs and outputs use plain record subtyping: a worker ignores input fields it does not know, and the runtime ignores output fields the contract does not declare. In practice, the only shape changes that keep a contract version are adding or removing a field that is optional (`T?`) or has a default. Anything else is an `error`:

```console
$ orbit compat c1.orbit c2.orbit
ok      workflow Checkout 1.0.0 → 1.1.0: definition changed; input and output unchanged
error   contract payments.charge@1.0.0: input.currency: (added) → String: required field added without a default; contract shape changed without a version change, so workers announcing payments.charge@1.0.0 no longer match every workflow that uses it; hint: give the changed contract a new version (1.0.0 → 2.0.0)
ok      contract payments.charge@1.0.0: output.receipt_url: (added) → String?: field added; compatible for old and new workers under the same version
error: compatibility check failed: 1 error(s), 0 warning(s)
```

A contract change also changes the definition hash of every workflow that uses it, so those workflows need a new version as well. `orbit compat` reports that through the workflow rules.

### New contract version

A new contract version never reaches old workers, so it cannot break anything at runtime. `orbit compat` compares it with the closest lower old version and asks one question: can a worker built for the old version also serve the new one? That holds when the new input is assignable to the old input (an old worker can read what new workflows send) and the old output is assignable to the new output (new workflows accept what an old worker returns).

| Change in a new contract version | Result with a major bump | Result without a major bump |
|---|---|---|
| An old worker can serve the new version | `ok` | `ok` |
| An old worker cannot serve the new version | `info`: deploy workers for the new version before the workflows that use it | `warning`, with a hint naming the next major version |

This is a warning rather than an error because exact version routing already keeps old workers away from the new version. The warning keeps contract versions meaningful to the people who implement workers.

```console
$ orbit compat c1.orbit c3.orbit
ok      workflow Checkout 1.0.0 → 1.1.0: definition changed; input and output unchanged
ok      contract payments.charge 1.0.0 → 1.1.0: input.currency: (added) → String: field added; an implementation of 1.0.0 also satisfies 1.1.0
warning contract payments.charge 1.0.0 → 1.1.0: output.settled_at: (added) → Timestamp: required field added without a default; an implementation of 1.0.0 cannot serve 1.1.0, but the version bump is not major; hint: bump the major version (1.0.0 → 2.0.0)
info    contract payments.charge@1.0.0: contract version no longer declared or used; keep workers for it until executions that use it finish
```

Contract versions that exist only in the old version, and contracts that exist only on one side, are reported as `info`.

## Using it in CI

Compare each changed workflow file with its version on the main branch:

```bash
git show main:workflows/invoice.orbit > old.orbit
orbit compat old.orbit workflows/invoice.orbit
```

A file that imports other files must be compared together with them. Check out the old tree in a separate directory and point `orbit compat` at the entry file there:

```bash
git worktree add ../orbit-main main
orbit compat ../orbit-main/workflows/invoice.orbit workflows/invoice.orbit
```

You can also keep the IR you deployed (`orbit compile -o deployed/`) and compare new sources against it, so the check reflects what is actually registered:

```bash
orbit compat deployed/invoice.orbit.json workflows/invoice.orbit
```

As a GitHub Actions step:

```yaml
- name: Check workflow compatibility
  run: |
    git fetch origin main
    git show origin/main:workflows/invoice.orbit > old.orbit
    orbit compat old.orbit workflows/invoice.orbit
```

## JSON format

With `--format json`, stdout holds one object:

```json
{
  "compatible": false,
  "errors": 2,
  "warnings": 0,
  "findings": [
    {
      "severity": "error",
      "kind": "workflow",
      "name": "Invoice",
      "old_version": "1.4.0",
      "new_version": "1.5.0",
      "path": "input.amount",
      "old_type": "Float",
      "new_type": "Int",
      "message": "incompatible type: expected `Int`, found `Float`; breaking for callers without a major version bump",
      "hint": "bump the major version (1.4.0 → 2.0.0)"
    },
    {
      "severity": "error",
      "kind": "workflow",
      "name": "Invoice",
      "old_version": "1.4.0",
      "new_version": "1.5.0",
      "path": "input.currency",
      "new_type": "String",
      "message": "required field added without a default; breaking for callers without a major version bump",
      "hint": "bump the major version (1.4.0 → 2.0.0)"
    },
    {
      "severity": "ok",
      "kind": "workflow",
      "name": "Invoice",
      "old_version": "1.4.0",
      "new_version": "1.5.0",
      "path": "output.total",
      "old_type": "Float",
      "new_type": "Int",
      "message": "narrowed; still accepted where the old type is expected"
    }
  ]
}
```

| Field | Type | Meaning |
|---|---|---|
| `compatible` | bool | `true` when there are no errors. |
| `errors`, `warnings` | integer | Number of findings with that severity. |
| `findings[].severity` | string | `error`, `warning`, `ok` or `info`. |
| `findings[].kind` | string | `workflow` or `contract`. |
| `findings[].name` | string | Workflow or contract name. |
| `findings[].old_version`, `new_version` | string | Absent on the side where the workflow or contract does not exist. |
| `findings[].path` | string | Field path, starting with `input`, `output`, `signal <name>` or `query <name>`; absent for findings about a whole workflow or contract. |
| `findings[].old_type`, `new_type` | string | Types at `path`; absent when the field does not exist on that side. |
| `findings[].message` | string | What changed and why it matters. |
| `findings[].hint` | string | Suggested fix, when there is one. |

Findings list workflows first, then contracts, each sorted by name.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | No errors. Warnings, `ok` and `info` findings are allowed. |
| `1` | At least one `error` finding. In `human` format, stderr ends with `error: compatibility check failed: <n> error(s), <m> warning(s)`. |
| `2` | Usage error: a file cannot be read, a `.json` file is not valid IR, a `.orbit` file has compile errors (the diagnostics are printed on stderr), or an argument is missing. |

## See also

- [Versioning](../language/versioning.md)
- [Signals and queries](../language/signals.md)
- [Task contracts](../language/tasks.md#changing-a-contract)
- [CLI: orbit compat](../reference/cli.md#orbit-compat)
- [Formatter](fmt.md)
