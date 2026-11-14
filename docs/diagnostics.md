# Compiler diagnostics

Every ORBIT diagnostic has a stable code. Codes never change meaning; retired
codes are not reused. `orbit check --format json` emits the same information
for editors and CI (code, severity, file, line, column, end position, labels,
notes and help).

```text
error[E0300]: unknown worker task `risk.analize`
 --> workflows/order.orbit:4:19
  |
4 |   step a { worker "risk.analize" }
  |                   ^^^^^^^^^^^^^^
  = help: did you mean `risk.analyze`?
```

## Lexical (E00xx)

| Code  | Meaning |
|-------|---------|
| E0001 | Unexpected character |
| E0002 | Unterminated string literal (strings are single-line) |
| E0003 | Unterminated block comment |
| E0004 | Unknown or invalid escape sequence |
| E0005 | Unknown duration unit (valid: `ms`, `s`, `m`, `h`, `d`) |
| E0006 | Integer literal does not fit in 64 bits |

## Syntax (E01xx)

| Code  | Meaning |
|-------|---------|
| E0100 | Expected a specific token |
| E0101 | Duration is missing a unit (`timeout 5` → `timeout 5s`) |
| E0102 | Unknown member in a block (with "did you mean" suggestions) |
| E0103 | Unclosed `{` |
| E0104 | Expected a top-level declaration |
| E0105 | `on event` / `on failure` handlers are reserved but not supported in v0.1 |
| E0106 | Duplicate member, or a step with both `worker` and `sleep` |
| E0107 | Duplicate field in an object literal |

## Declarations and types (E02xx)

| Code  | Meaning |
|-------|---------|
| E0200 | Duplicate workflow/schema/task, redefinition of a built-in type, or a reserved `orbit.` task name |
| E0201 | Unknown type |
| E0202 | Recursive schema |
| E0203 | Duplicate field in a record type |
| E0204 | Map key type is not `String` |
| E0205 | Invalid default value (must be a constant of the field's type) |
| E0206 | Import or source file cannot be read |
| W0201 | Version is not `MAJOR.MINOR.PATCH` |

## Steps and dependencies (E03xx)

| Code  | Meaning |
|-------|---------|
| E0300 | Unknown worker task (no `task` contract declared) |
| E0301 | Step has neither `worker` nor `sleep` |
| E0302 | Duplicate step name |
| E0303 | `after` references an unknown step |
| E0304 | Circular dependency (the cycle is printed) |
| E0305 | Step depends on, or reads, itself |
| E0306 | Reserved step name (`input`, `execution`, `output`, `if`, `then`, `else`) |
| E0307 | Member not allowed here (e.g. `retry` on a `sleep` step, `when` in `compensate`) |
| E0308 | Invalid compensation (on a `sleep` step, without a worker, or reading a step that is not an ancestor) |
| E0310 | Task input/output is not a record type |
| W0301 | Unknown worker language in `using` |

## Expressions (E04xx)

| Code  | Meaning |
|-------|---------|
| E0400 | Type mismatch |
| E0401 | Unknown reference (not `input`, `execution` or a step) |
| E0402 | Unknown field on a type |
| E0403 | Missing required field(s) |
| E0404 | Unknown field in an object literal for a record |
| E0405 | Optional value used where a value is required — use `coalesce(x, default)` |
| E0406 | Operator not defined for the operand types |
| E0407 | Unknown function or wrong number of arguments |
| E0408 | Condition is not `Bool` |
| E0410 | Invalid retry policy or timeout |

## Workflows (E05xx)

| Code  | Meaning |
|-------|---------|
| E0500 | Workflow has no `version` |
| E0501 | Workflow declares `output` with required fields but has no `return` |
