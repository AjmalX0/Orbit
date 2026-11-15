# Compiler diagnostics

Every diagnostic the ORBIT compiler reports has a stable code. A code never changes meaning, and retired codes are not reused, so you can search for a code, link to it, or match it in CI. This page lists every code with its meaning and how to fix it. Codes starting with `E` are errors and stop compilation; codes starting with `W` are warnings, which `orbit check --strict` and `orbit compile --strict` treat as errors.

## Reading a diagnostic

`orbit check`, `orbit compile` and the language server report the same diagnostics:

```text
error[E0300]: unknown worker task `risk.analize`
 --> workflows/order.orbit:4:19
  |
4 |   step a { worker "risk.analize" }
  |                   ^^^^^^^^^^^^^^
  = help: did you mean `risk.analyze`?
```

The first line has the severity, the code and the message. The `-->` line gives the file, line and column. Labels mark the source, `= note:` lines add context (a cycle, for example), and `= help:` lines suggest a fix.

`orbit check --format json` prints the same information for editors and CI: code, severity, file, line, column, end position, labels, notes and help.

```json
{
  "diagnostics": [
    {
      "severity": "error",
      "code": "E0300",
      "message": "unknown worker task `greeting.compose`",
      "location": { "file": "messy.orbit", "line": 6, "column": 25 },
      "end_line": 6,
      "end_column": 43,
      "labels": [],
      "notes": [],
      "help": "declare its contract: `task \"greeting.compose\" { version \"1.0.0\" input { ... } output { ... } }`"
    }
  ]
}
```

## Code ranges

| Range | Area |
|---|---|
| [E00xx](#lexical-e00xx) | Lexical: characters, strings, comments, literals. |
| [E01xx](#syntax-e01xx) | Syntax: tokens, blocks and members. |
| [E02xx, W02xx](#declarations-and-types-e02xx) | Declarations, schemas, types and imports. |
| [E03xx, W03xx](#steps-and-dependencies-e03xx) | Steps, dependencies, tasks and child workflows. |
| [E04xx](#expressions-e04xx) | Expressions, types of values, retry and timeout settings. |
| [E05xx](#workflows-e05xx) | Workflow-level rules. |
| [W06xx](#static-safety-w06xx) | Static safety checks: compensations, constant conditions, signals and waits. |
| [E07xx, W07xx](#tests-e07xx-w07xx) | `test` declarations: workflows, mocks, signals and expectations. |

## Lexical (E00xx)

### E0001: unexpected character

The source contains a character that cannot start any token, for example `#` or `$` outside a string. Message: ``unexpected character `#` ``. Remove the character, or put it inside a string literal.

### E0002: unterminated string literal

A string literal has no closing `"`. Strings are single-line, so a line break also ends the string with this error. Close the string on the same line; use `\n` for a line break inside a string.

### E0003: unterminated block comment

A `/*` comment has no matching `*/`. Close the comment.

### E0004: invalid escape sequence

A string contains an unknown escape (``unknown escape sequence `\q` ``) or an invalid Unicode escape (``invalid unicode escape `\u{110000}` ``). Use one of the supported escapes, or escape the backslash as `\\`.

### E0005: unknown duration unit

A duration literal has a unit ORBIT does not know, for example `5sec`. The valid units are `ms`, `s`, `m`, `h` and `d`.

### E0006: integer literal out of range

An integer literal does not fit in a signed 64-bit integer. Use a smaller value, or a `Float`.

## Syntax (E01xx)

### E0100: expected token

The parser expected a specific token and found another. Message: `expected <token>, found <token>`. Check for a missing `{`, `}`, `,` or `:` immediately before the reported position.

### E0101: duration is missing a unit

A duration was written as a bare number, for example `timeout 5`. Add a unit: `timeout 5s`. Help: ``add a unit such as `ms`, `s`, `m`, `h` or `d`, e.g. `30s` ``.

### E0102: unknown member

A block contains a member that does not belong there, for example a misspelled `wroker` in a step. The compiler suggests the closest valid member (``did you mean `worker`?``).

### E0103: unclosed block

A `{` has no matching `}` before the end of the file. Message: ``unclosed `{` in <context>``. The label points at the opening brace.

### E0104: expected a declaration

At the top level of a file, only `import`, `schema`, `task` and `workflow` declarations are allowed. Message: `expected a declaration, found <token>`.

### E0105: unsupported handler

An `on` member names a handler kind other than `failure`, `cancel`, `timeout` or `event`. Message: ``` `on success` handlers are not supported ```, with help ``use `on failure`, `on cancel`, `on timeout` or `on event <signal>` ``.

```text
error[E0105]: `on success` handlers are not supported
 --> notify.orbit:4:5
  |
4 |     on success {
  |     ^^^^^^^^^^^^
  = help: use `on failure`, `on cancel`, `on timeout` or `on event <signal>`
```

To run steps after a successful execution, make them ordinary steps that depend on the others. See [Outcome handlers](../language/handlers.md).

### E0106: duplicate member

A member appears twice in the same block (``duplicate `timeout` ``), or a step has more than one action (``a step can only have one `worker`, `workflow`, `sleep`, `await` or `compute` ``). Keep one, or split the work into separate steps.

### E0107: duplicate field in object literal

An object literal sets the same field twice, for example `{ id: 1, id: 2 }`. Message: ``duplicate field `id` in object``.

## Declarations and types (E02xx)

### E0200: duplicate declaration

A workflow, schema or task is declared more than once (across all imported files), a schema redefines a built-in type such as `String`, or a task name starts with the reserved prefix `orbit.` (``task names starting with `orbit.` are reserved for built-in tasks``). Rename one of the declarations.

### E0201: unknown type

A type name is not a built-in type and not a declared schema. Message: ``unknown type `Adress` ``. Declare the schema, import the file that declares it, or fix the spelling.

### E0202: recursive schema

A schema contains itself, directly or through other schemas. The note prints the cycle (`cycle: A -> B -> A`). Break the cycle, for example by storing an id instead of the nested record.

### E0203: duplicate field in a record type

A schema or inline record type declares the same field twice. Message: ``duplicate field `id` ``.

### E0204: map key is not String

Map keys must be `String`. Message: ``map keys must be `String`, found `Int` ``.

### E0205: invalid default value

A field default is not a constant of the field's type, for example `count: Int = "one"`. Message: `invalid default value: <reason>`.

### E0206: file cannot be read

An imported file, or a file given on the command line, cannot be read. Messages: ``cannot import `contracts.orbit`: <reason>`` and ``cannot read `<path>`: <reason>``. Import paths are relative to the importing file.

### W0201: version is not semantic

A workflow or task version is not `MAJOR.MINOR.PATCH`. Message: ``version `1.0` is not semantic (MAJOR.MINOR.PATCH)``. The runtime orders versions semantically when you start a workflow without a version, so non-semantic versions may not sort as you expect.

## Steps and dependencies (E03xx)

### E0300: unknown worker task

A step names a task with `worker "..."` that has no `task` contract in scope. The compiler suggests the closest declared task (``did you mean `risk.analyze`?``), or tells you how to declare one. Declare the contract, or import the file that declares it.

### E0301: step does nothing

A step has no action. Message: ``step `a` does nothing``, with help ``add `worker "task.name"`, `workflow "Name"`, `sleep 10s`, `await <signal>` or `compute <expression>` ``. Add one of them.

### E0302: duplicate step name

Two steps in the same workflow, or in the same handler block, have the same name. Message: ``step `a` is defined more than once``. A handler step that has the name of a workflow step, of a reserved name or `failure` gets ``handler step `a` has the same name as a workflow step or a reserved name``, with help ``give handler steps their own names, e.g. `alert_ops` ``.

### E0303: unknown step in after

An `after` clause names a step that does not exist. Message: ``` `after` references unknown step `validte` ```. Inside an `on failure`, `on cancel` or `on timeout` block, `after` may only name steps of the same block: ``` `after` in an `on cancel` block must name a step of the same block ```, with the note `workflow steps have all finished before a handler runs`.

### E0304: circular dependency

Steps depend on each other in a cycle, through `after` or through data references. The note prints the cycle. Remove one of the dependencies. Between the steps of a handler block, the message is `circular dependency between handler steps`.

### E0305: step depends on itself

A step lists itself in `after` (``step `a` cannot run after itself``) or reads its own output (``step `a` cannot read its own output``). A step's output can be read by later steps, its `compensate` block, or `return`.

### E0306: reserved step name

A step is named `input`, `execution`, `output`, `if`, `then` or `else`. These names are reserved because expressions use them. Rename the step.

### E0307: member not allowed here

A member is valid ORBIT but not in this position. Examples: `retry` on a `sleep` step, `when` or `for` inside `compensate`, `retry`, `using`, `idempotency_key` or `queue_timeout` on a `workflow` step, `for` on a `sleep` step, and `max_parallel` without `for` (help: ``add `for item in <list>` to fan the step out``). An `await` step takes only `after`, `when` and `timeout` (``` `retry` is not allowed in an `await` step ```; for `for`, the note ``an `await` step receives one signal``). A `compute` step takes no `with`, `using`, `timeout`, `queue_timeout`, `retry` or `idempotency_key` (``` `timeout` is not allowed in a `compute` step ```). Handler steps take no `await` (``` `await` is not allowed in an `on timeout` step ```, with the note `handlers run while the execution is finishing; it no longer accepts signals`), no child workflow (``child workflows are not allowed in an `on failure` step``), no `for` and no `max_parallel`. See [Signals and queries](../language/signals.md#members-of-an-await-step), [Compute steps](../language/compute.md#members) and [Outcome handlers](../language/handlers.md#rules).

### E0308: invalid compensation

A compensation cannot run as written: a `sleep`, `await` or `compute` step has a `compensate` block (``a `compute` step has nothing to compensate``), the block has no `worker` (``` `compensate` block needs a `worker` ```), it calls something other than a worker task (`compensation must call a worker task`), or it reads a step that is not an ancestor of the compensated step. Steps of an `on failure`, `on cancel` or `on timeout` block cannot be compensated: ``handler steps cannot declare `compensate` ``.

### E0310: task input or output is not a record

A task's `input` or `output` must be a record type: an inline `{ ... }` or a schema. Message: ``task input must be a record type, found `String` ``.

### E0311: workflows start each other in a cycle

Child workflows form a cycle, for example `A` starts `B` and `B` starts `A`. Message: `workflows call each other in a cycle`; the note prints the cycle.

### E0312: unknown child workflow

A `workflow "Name"` step names a workflow that is not declared in scope. The compiler suggests the closest name.

### E0315: unknown signal

An `await` step names a signal the workflow does not declare. Message: ``unknown signal `reveiw` ``, with help ``did you mean `review`?`` for a close name, or ``declare it in the workflow: `signal approval { ... }` `` otherwise.

```text
error[E0315]: unknown signal `reveiw`
 --> approvals.orbit:5:27
  |
5 |     step decision { await reveiw }
  |                           ^^^^^^
  = help: did you mean `review`?
```

Signals are declared per workflow; a signal of another workflow is not in scope.

### E0316: unknown compensated_by task

A task contract's `compensated_by` names a task that is not declared in the compilation. Message: ``` `compensated_by` names unknown task `payments.refnud` ```, with help ``did you mean `payments.refund`?``. Declare the task or import the file that declares it. See [Static safety checks](../language/safety.md#declaring-how-a-task-is-undone-compensated_by).

### E0317: duplicate signal

A workflow declares two signals with the same name. Message: ``signal `review` is declared more than once``; a label marks the first declaration. Merge the two payloads into one declaration, or rename one.

### E0318: duplicate query

A workflow declares two queries with the same name. Message: ``query `state` is declared more than once``; a label marks the first declaration.

### E0319: duplicate handler

A workflow declares two `on` blocks of the same kind, or two `on event` blocks for the same signal. Message: ``` `on failure` is declared more than once ```; a label marks the first declaration. Merge the steps into one block.

```text
error[E0319]: `on failure` is declared more than once
 --> notify.orbit:7:8
  |
7 |     on failure {
  |        ^^^^^^^
4 |     on failure {
  |        ------- first declared here
```

### E0320: unknown limit

A `limits` block names a field other than `max_fan_out`, `max_history_events` and `max_payload_bytes`. A close match is suggested.

```text
error[E0320]: unknown limit `max_fanout`
 --> lim.orbit:3:14
  |
3 |     limits { max_fanout: 10, max_history_events: 0 }
  |              ^^^^^^^^^^
  = help: did you mean `max_fan_out`?
```

### E0321: invalid limit value

A limit is zero, negative, or not an integer literal.

```text
error[E0321]: limit `max_history_events` must be a positive integer literal
 --> lim.orbit:3:50
  |
3 |     limits { max_fanout: 10, max_history_events: 0 }
  |                                                  ^
  = help: for example `max_fan_out: 100`
```

### E0322: signal both awaited and handled

A signal handled by an `on event` block is also the target of an `await` step. Every arriving signal of that name starts the block, so the `await` could never receive one. Use a separate signal for the `await`.

```text
error[E0322]: signal `note` is handled by `on event` and cannot also be awaited
 --> aw.orbit:4:20
  |
4 |     step a { await note timeout 1h }
  |                    ^^^^
5 |     on event note {
  |              ---- handled here
  = note: every signal handled by `on event` starts the handler; an `await` would compete for it
```

### W0301: unknown worker language

A `using` clause names a language ORBIT does not recognize. Message: ``unknown worker language `pyhton` ``. The step still compiles, but it is dispatched only to workers that announce exactly that runtime.

## Expressions (E04xx)

### E0400: type mismatch

A value has the wrong type. Message: ``type mismatch: expected `Int`, found `String` ``. The same code reports string literals that are not a valid UUID or RFC 3339 timestamp where a `Uuid` or `Timestamp` is expected.

### E0401: unknown reference

An expression names something that is not `input`, `execution`, a step, or a loop variable in scope. `failure` is in scope only in the steps of `on failure`, `on cancel` and `on timeout` blocks; elsewhere it is ``unknown reference `failure` ``. It is also reported when a `for` loop variable is used in the step's `when` condition (``loop variable `item` is not in scope in `when` ``): `when` decides once, before the step fans out, whether the whole step runs.

### E0402: unknown field

A member access names a field the type does not have. Message: ``no field `adress` on type `{ address: String, n: Int }` ``, with help ``did you mean `address`?``.

### E0403: missing required fields

An object literal for a record does not set every required field, or a step whose task requires input has no `with` (``task `orders.validate` requires input but the step has no `with` ``). Set the missing fields, give them defaults, or declare them optional with `?`.

### E0404: unknown field in object literal

An object literal sets a field the target record does not declare. Message: ``unknown field `colour` for `Item` ``.

### E0405: optional value where a value is required

A value that may be `null` is used where a non-optional type is required. Message: ``value may be `null` but `String` is required``. Provide a fallback with `coalesce(value, default)`, or declare the target as optional with `?`.

### E0406: operator not defined for operand types

An operator is applied to types it does not support. Message: ``operator `*` cannot be applied to `String` and `Int` ``.

### E0407: unknown function or wrong arity

A function call names a function that does not exist, or passes the wrong number of arguments. See [Built-in functions](../language/functions.md).

### E0408: condition is not Bool

A `when` condition or an `if` expression's condition does not have type `Bool`. Messages: ``` `when` condition must be `Bool`, found `String` ``` and ``` `if` condition must be `Bool`, found `Int` ```.

### E0410: invalid retry, timeout or max_parallel

A retry policy, timeout or `max_parallel` value is out of range or malformed. Messages include: `timeouts must be greater than zero`, ``` `attempts` must be between 1 and 1000 ``` (note: `attempts` counts every execution, including the first), ``unknown backoff strategy `linear` ``, ``unknown retry setting `max_attempts` ``, ``duplicate retry setting `attempts` ``, `error codes must be string literals`, ``` `max_delay` must be at least `initial_delay` ``` and ``` `max_parallel` must be between 1 and 10000 ```.

### E0413: loop variable or lambda parameter shadows a name

The variable of a `for` loop has the same name as a step or a built-in name such as `input`. Message: ``loop variable `input` shadows <what>``. The same code reports a lambda parameter that reuses the name of a step, a loop variable, an enclosing lambda parameter, `input` or `execution`: ``lambda parameter `base` shadows a name already in scope``. Rename the variable or parameter.

### E0414: for over something that is not a list

The expression after `for <var> in` does not have a `List` type. Message: ``` `for` needs a list, found `Int` ```.

### E0415: lambda outside a collection function

A lambda (`x => ...`) appears somewhere other than the second argument of `map`, `filter`, `find`, `any` or `all`. Message: ``a lambda can only be passed to `map`, `filter`, `any`, `all` or `find` ``, with help ``for example `map(input.items, item => item.price)` ``. Lambdas are not values: they cannot be stored in a field, returned or passed to other functions. See [Expressions](../language/expressions.md#lambdas).

## Workflows (E05xx)

### E0500: workflow has no version

Every workflow needs a `version`. Message: ``workflow `Order` has no version``. Help: ``add `version "1.0.0"`; running executions stay pinned to the version they started with``.

### E0501: output without return

The workflow declares an `output` with required fields but has no `return`. Message: ``workflow declares an `output` but has no `return` ``. Add a `return` that builds the output.

## Static safety (W06xx)

These warnings flag workflows that compile but are likely to misbehave. [Static safety checks](../language/safety.md) has a full example of each.

### W0601: missing compensation

A step calls a task whose contract declares `compensated_by`, and the step has no `compensate` block. Message: ``step `charge` calls `payments.charge` but has no compensation``, with the note ``` `payments.charge` declares `compensated_by "payments.refund"`: if a later step fails, its effect would be left in place ``` and help ``add `compensate { worker "payments.refund" with { ... } }` ``.

### W0602: compensation uses a different task

The step's `compensate` block calls another task than the contract's `compensated_by`. Message: ``step `charge` compensates `payments.charge` with `payments.void`, but the contract names `payments.refund` ``. Use the named task, or change the contract.

### W0603: step never runs

A `when` condition reads no input, step output, execution field or variable, and evaluates to `false` or `null`. Message: ``step `legacy_audit` never runs: its `when` condition is always false``, with help `remove the step, or make the condition depend on input or step results`.

### W0604: condition always true

A `when` condition reads nothing and evaluates to `true`. Message: ``the `when` condition of step `audit` is always true``, with help ``remove `when`; it also makes the step's output optional for no reason``.

### W0605: signal declared but never awaited

No step awaits a declared signal. Message: ``signal `escalate` is declared but no step awaits it``, with help ``add a step with `await escalate`, or remove the declaration``. The runtime still accepts the signal, but nothing reads it.

### W0606: wait without a timeout

An `await` step has no `timeout`, and the workflow has no `timeout`. Message: ``step `decision` waits for signal `review` with no timeout``, with help ``add `timeout 7d` to the step (or a workflow `timeout`) so the execution cannot wait forever``.

## Tests (E07xx, W07xx)

These diagnostics come from `test` declarations. They are reported by every command that compiles the file, including `orbit check`, and a file with test errors cannot be deployed. Values in a test are also checked with the expression rules above: a missing required `input` field is `E0403`, a mock result of the wrong type is `E0400`, and reading `input` in a test `input` or `expect` value is `E0401`. [Testing workflows](../tooling/testing.md) describes the declarations.

### E0700: unknown workflow in a test

The test's `workflow` is not defined in, or imported into, the compilation. Message: ``unknown workflow `OrderAproval` ``, with help ``did you mean `OrderApproval`?``, or help ``import the file that defines it, e.g. `import "orders.orbit"` `` when no name is close.

### E0701: test without a workflow

Message: ``test `no workflow` does not name a workflow``, with help ``add `workflow <Name>` to the test``.

### E0702: test version does not match

The test names a `version` other than the one the sources define. Message: ``test expects `OrderApproval` version 2.0.0, but the sources define version 1.0.0``. Update the version, or remove it to test whatever version is compiled.

### E0703: mocked task is not called by the workflow

A `mock` names a task that the workflow does not call in a step, a compensation or one of its child workflows. Message: ``` `OrderApproval` does not call task `payments.charg` ```, with a suggestion, or a note listing the tasks it can call.

### E0704: task mocked twice

Message: ``task `payments.charge` is mocked more than once``, with help ``chain behaviours instead: `fails "X" times 1 then returns { ... }` ``.

### E0705: unknown signal in a test

A `signal` names a signal the workflow does not declare. Message: ``` `OrderApproval` declares no signal `reveiw` ```, with help ``did you mean `review`?``.

### E0706: unknown step in an expectation

Message: ``` `OrderApproval` has no step `chrge` ```, with help ``did you mean `charge`?``, or a note listing the steps.

### E0707: unknown status

`expect status` accepts `COMPLETED`, `FAILED`, `CANCELLED` and `TIMED_OUT`. `expect step <name> <STATUS>` accepts the unit statuses `PENDING`, `RUNNING`, `SCHEDULED`, `DISPATCHED`, `ATTEMPT_FAILED`, `UNCERTAIN`, `RETRY_WAIT`, `WAITING`, `SUCCEEDED`, `SKIPPED`, `FAILED` and `CANCELLED`. Message: ``` `COMPLETE` is not a workflow status ```, with help ``did you mean `COMPLETED`?``; a lower-case status gets the help ``statuses are upper case: `COMPLETED` ``.

### E0708: expected compensation of a step without one

`expect compensated` names a step without a `compensate` block. Message: ``step `notify` has no `compensate` block``.

### E0709: duplicate test name

Message: ``test `errors` is declared more than once``, with a label at the first declaration. Test names are unique across the compilation, so `--filter` and reports identify a test unambiguously.

### E0710: test value cannot be evaluated

A constant in `input`, a `signal` payload or an expected output type-checks but fails to evaluate, for example a division by zero. Message: ``cannot evaluate a test input: <reason>``.

### E0711: invalid count

`times` must be at least 1, and `attempts` must be a non-negative number. Message: ``` `times 0` must be at least 1 ```.

### W0701: test checks nothing

The test has no `expect`. Message: ``test `nothing` checks nothing``, with help ``add an expectation such as `expect status COMPLETED` ``. Such a test still fails if the execution cannot finish, for example because a task is not mocked.

## See also

- [CLI: orbit check](cli.md#orbit-check)
- [Static safety checks](../language/safety.md)
- [Testing workflows](../tooling/testing.md)
- [Editor support](../tooling/editors.md)
- [Grammar](../language/grammar.md)
- [Expressions](../language/expressions.md)
- [Types](../language/types.md)
