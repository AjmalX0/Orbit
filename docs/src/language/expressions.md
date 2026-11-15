# Expressions

Expressions compute step inputs, conditions, idempotency keys, fan-out lists
and workflow results from the workflow input and the outputs of earlier steps.
They are pure and deterministic: they perform no I/O and cannot read the
clock, so every decision the runtime makes can be replayed from the execution
history. This page is the reference for literals, references, operators,
precedence and the evaluation rules, including the behavior with `null`.

## Where expressions appear

| Position | Expected type | Can read |
|----------|---------------|----------|
| step `with` | the task or child workflow input | `input`, `execution`, steps, loop variable |
| step `when` | `Bool` or `Bool?` | `input`, `execution`, steps |
| step `idempotency_key` | `String` | `input`, `execution`, steps, loop variable |
| step `for x in ...` | a list | `input`, `execution`, steps |
| step `compute` | any (the type becomes the step's output type) | `input`, `execution`, steps, loop variable |
| `compensate` members | as above | `input`, `execution`, the step and its ancestors, loop variable |
| workflow `return` | the workflow `output` | `input`, `execution`, steps |
| `query name = ...` | any | `input`, `execution`, steps (every step output optional) |
| field defaults (`= ...`) | the field type | constants only |
| `retry` settings | literals only | nothing |

## Literals

| Literal | Type | Examples |
|---------|------|----------|
| string | `String` | `"text"`, `"line\nbreak"`, `"\u{1F680}"` |
| integer | `Int` | `0`, `42`, `1_000_000` |
| float | `Float` | `3.14`, `0.5`, `1_000.25` |
| duration | `Duration` | `500ms`, `30s`, `1.5m`, `2h`, `1d` |
| boolean | `Bool` | `true`, `false` |
| null | `null` | `null` |
| object | record | `{ id: input.id, "content-type": "text/plain" }` |
| list | `List<T>` | `[1, 2, 3]`, `["a", "b"]`, `[]` |

Notes:

- Strings are single-line. The escapes are `\n`, `\t`, `\r`, `\"`, `\\` and
  `\u{X...}` with one or more hexadecimal digits. Any other escape is `E0004`,
  and a line break inside a string is `E0002`.
- Underscores in numbers are ignored (`1_000` is `1000`).
- A float needs digits on both sides of the point: `0.5`, not `.5` or `5.`.
  There is no exponent notation.
- A leading `-` is an operator. `-5` is folded into a negative literal.
  Because the literal `9223372036854775808` does not fit in 64 bits, write the
  minimum `Int` as `-9223372036854775807 - 1`.
- A number directly followed by letters is a duration; an unknown unit is
  `E0005` (`5q`).

## References

An identifier at the start of an expression is a reference. It resolves, in
order, to:

1. a loop variable bound by the enclosing `for` step,
2. `input`, the workflow input,
3. `execution`, the execution metadata,
4. a step of the workflow.

Anything else is `E0401`, with a suggestion when a name is close.

| Expression | Type | Value |
|------------|------|-------|
| `input` | the workflow input record | the whole input |
| `input.order_id` | the field's type | a field of the input |
| `analyze_risk` | the step's output type | the whole output of the step |
| `analyze_risk.score` | the field's type | a field of the step output |
| `item.sku` | the element's field type | a field of the current element |
| `execution.id` | `String` | the execution id |
| `execution.workflow` | `String` | the workflow name |
| `execution.version` | `String` | the workflow version |

`execution` must be followed by exactly one of `id`, `workflow` or `version`.
`execution` alone, or another field, is `E0402`.

The type of a step reference depends on the step: optional if the step has a
`when`, a list if it has a `for`. See [Steps](steps.md#step-output-types).

## Member access

`a.b` reads field `b` of a record. An unknown field is `E0402`, with the
closest field suggested or the available fields listed.

Member access propagates `null`: if `a` is `null`, `a.b` is `null`. The type
of `a.b` is therefore optional whenever `a` is optional, which is how a
skipped step's fields become optional.

Member access also works on maps, as a shorthand for indexing with a string:
`headers.accept` is `headers["accept"]`, and its type is optional because the
key may be missing.

## Indexing

`target[index]` reads an element:

| Target type | Index type | Result type | Runtime behavior |
|-------------|------------|-------------|------------------|
| `List<T>` | `Int` | `T?` | Negative indexes count from the end (`-1` is the last element). Out-of-range indexes give `null`. |
| `Map<String, T>` | `String` | `T?` | A missing key gives `null`. |
| a record | a string literal | the field type | `r["field"]` is the same as `r.field`; a non-literal index is `E0406`. |
| `JSON` | any | `JSON` | |
| `T?` | as for `T` | as for `T`, optional | A `null` target gives `null`. |
| other types | | | `E0406`: the type cannot be indexed |

A field access after an index or a call reads a field of the result:
`input.items[0].sku` has type `String?`.

## Object literals

```orbit
workflow Objects {
    version "1.0.0"
    input { order_id: String, amount: Float }
    return {
        id: input.order_id
        "content-type": "application/json"
        totals: { net: input.amount, gross: input.amount * 1.2 }
    }
}
```

- Keys are identifiers or string literals. Use a string for keys that are
  not identifiers, such as `"content-type"`, or for `true`, `false` and
  `null`.
- A key may appear only once (`E0107`).
- Entries are separated by commas, semicolons or line breaks.
- There is no shorthand: `{ order_id }` is a syntax error; write
  `{ order_id: input.order_id }`.

How an object literal is typed depends on where it appears:

| Expected type | Rule |
|---------------|------|
| a record | Each entry is checked against its field. Unknown keys are `E0404`, missing required fields are `E0403`. |
| `Map<String, T>` | Every value is checked against `T`. |
| none, or `JSON` | The literal's type is inferred as an anonymous record; a `null` entry has type `JSON`. |

## List literals

Items are separated by commas or line breaks; a trailing comma is allowed.
A semicolon between items is a syntax error. The item type is the combination
of all item types (see [Types](types.md#type-inference)), `[]` has type
`List<JSON>`, and incompatible items are `E0400`. Where a `List<T>` is
expected, each item is checked against `T`.

## Operators

From lowest to highest precedence. All binary operators are left-associative.

| Precedence | Operators | Operand types | Result |
|------------|-----------|---------------|--------|
| 1 | `\|\|` | `Bool`, `Bool` | `Bool` |
| 2 | `&&` | `Bool`, `Bool` | `Bool` |
| 3 | `==` `!=` | two compatible types | `Bool` |
| 4 | `<` `<=` `>` `>=` | two numbers, or two strings | `Bool` |
| 5 | `+` | two numbers; two strings; two lists | number, `String`, list |
| 5 | `-` | two numbers | number |
| 6 | `*` `/` `%` | two numbers | number |
| 7 (prefix) | `!` | `Bool` | `Bool` |
| 7 (prefix) | `-` | `Int` or `Float` | same type |
| 8 (postfix) | `.field`, `[index]`, `f(...)` | | |

`1 + 2 * 3 == 7 && !false` therefore groups as
`((1 + (2 * 3)) == 7) && (!false)`, and `10 - 3 - 2` is `(10 - 3) - 2`. Use
parentheses to group differently.

Operators that are not defined for their operand types are `E0406`, with
labels showing each operand's type. Optional operands are not accepted by
arithmetic, comparison or logical operators; the diagnostic suggests
`coalesce`. If either operand has type `JSON`, the check is skipped and the
operation is checked at runtime.

### Arithmetic

| Operands | Result type | Runtime behavior |
|----------|-------------|------------------|
| `Int` and `Int` | `Int` | 64-bit integer arithmetic. Overflow is an error. |
| `Int` and `Float`, or two `Float` | `Float` | 64-bit floating point. |
| either is `JSON` | `JSON` | Checked at runtime. |

- `/` on integers truncates toward zero: `7 / 2` is `3` and `-7 / 2` is `-3`.
- `%` takes the sign of the dividend: `-7 % 3` is `-1`.
- Division or remainder by zero, integer or float, is an error.
- A float result that is not finite becomes `null`.
- `Duration` has no arithmetic: `30s + 1s` is `E0406`.

The runtime chooses integer or float arithmetic from the actual JSON values,
not from the declared types. A `Float` field whose JSON value is a whole
number written without a decimal point, such as `5`, is an integer at runtime,
and `input.price / 2` then performs integer division (`2`). Write a float
literal (`input.price / 2.0`) or convert explicitly (`float(input.price) / 2`)
when you need float division.

### Concatenation

`+` on two strings concatenates them. `+` on two lists returns the items of
the left list followed by the items of the right list; the result type is the
combination of both list types. A string and a non-string is `E0406`, and the
diagnostic suggests [`string`](functions.md#string):

```orbit
workflow Label {
    version "1.0.0"
    input { order_id: String, attempt: Int }
    return { label: "order-" + input.order_id + "-" + string(input.attempt) }
}
```

### Comparison

`<`, `<=`, `>` and `>=` compare two numbers (`Int` and `Float` mix freely) or
two strings. Strings compare lexicographically by Unicode code point, so
`"B" < "a"` is `true`. Other types, including optional operands and
`Duration`, are `E0406`.

### Equality

`==` and `!=` compare values structurally: lists element by element, records
and maps key by key. Numbers compare by value, so `1 == 1.0` is `true`.

The operands must have compatible types: comparing an `Int` with a `String` is
`E0406` ("values of different types are never equal"). Comparing any value
with `null` is allowed, which is how you test an optional value:

```orbit
return { charged: charge.transaction_id != null }
```

### Logical operators

`&&` and `||` take `Bool` operands and short-circuit: the right operand is not
evaluated when the left one decides the result. `!` negates a `Bool`. A
`Bool?` operand is rejected at compile time (`E0406` for `&&`/`||`, `E0405`
for `!`); use `coalesce(flag, false)`. At runtime, a non-boolean operand
(possible only through `JSON`) is an error.

## Conditional expressions

```orbit
workflow Route {
    version "1.0.0"
    input { score: Float }
    return {
        queue: if input.score > 0.8 then "manual" else "auto"
    }
}
```

`if c then a else b` evaluates `c`, then exactly one branch.

- The condition must be `Bool` or `Bool?` (`E0408`). A `null` condition
  selects the `else` branch.
- `else` is required.
- The result type combines both branch types: `if c then 1 else 2.5` is
  `Float`, and `if c then "x" else null` is `String?`. Incompatible branches
  are `E0400`.
- `if`, `then` and `else` may be on separate lines.

## Function calls

`name(arg, ...)` calls a [built-in function](functions.md). There are no
user-defined functions. An unknown function or a wrong number of arguments is
`E0407`. The opening parenthesis must be on the same line as the name.

## Lambdas

A lambda is a one-parameter function, written `parameter => body`:

```orbit
workflow Prices {
    version "1.0.0"
    input { items: List<{ sku: String, price: Float, quantity: Int }> }
    return {
        totals: map(input.items, item => item.price * float(item.quantity))
        expensive: filter(input.items, item => item.price > 100.0)
        all_in_stock: all(input.items, item => item.quantity > 0)
    }
}
```

| Rule | Detail |
|---|---|
| Where | Only as the second argument of `map`, `filter`, `find`, `any` and `all`. Anywhere else is `E0415`. See [Collection functions](functions.md#collection-functions). |
| Parameter | One identifier. Its type is the element type of the list. It must not reuse the name of a step, a loop variable, an enclosing lambda parameter, `input` or `execution` (`E0413`). |
| Body | Any expression, up to the end of the argument. It can read `input`, `execution`, step outputs, loop variables and enclosing lambda parameters, so lambdas can nest: `map(a, x => map(b, y => x * y))`. |
| Body type | Any type for `map`; `Bool` for `filter`, `find`, `any` and `all`. |
| Line breaks | `=>` must be on the same line as the parameter. The body may start on the next line. |
| Precedence | `=>` binds loosest: `x => x * 2 + 1` is `x => ((x * 2) + 1)`. |

```orbit
workflow Totals {
    version "1.0.0"
    input { prices: List<Float> }
    step total {
        compute { doubled: p => p * 2.0 }
    }
}
```

```text
error[E0415]: a lambda can only be passed to `map`, `filter`, `any`, `all` or `find`
 --> lambda.orbit:5:28
  |
5 |         compute { doubled: p => p * 2.0 }
  |                            ^^^^^^^^^^^^
  = help: for example `map(input.items, item => item.price)`

error: compilation failed: 1 error(s), 0 warning(s)
```

A workflow that contains a lambda anywhere compiles to `orbit-ir/2`. See
[Versioning](versioning.md#ir-version).

## Line breaks and operators

An infix operator, and a postfix `.`, `[` or `(`, must be on the same line as
its left operand. A line break before one of these tokens ends the
expression. This is what lets object literals and blocks use line breaks as
separators:

```orbit
workflow Lines {
    version "1.0.0"
    input { a: Int, b: Int }
    return {
        sum: input.a +
            input.b          // continues: `+` ends the previous line
        a: input.a
        b: -input.b          // a new entry, not `input.a - input.b`
    }
}
```

Starting a line with an operator does not continue the previous line:

```orbit
workflow Broken {
    version "1.0.0"
    input { a: Int, b: Int }
    return {
        sum: input.a
            + input.b
    }
}
```

```text
error[E0100]: expected a field name or `}`, found `+`
 --> broken.orbit:6:13
  |
6 |             + input.b
  |             ^

error[E0102]: unexpected `+` in workflow
 --> broken.orbit:6:13
  |
6 |             + input.b
  |             ^

error[E0104]: expected a declaration, found `}`
 --> broken.orbit:8:1
  |
8 | }
  | ^
  = note: top-level declarations are `workflow`, `schema`, `task`, `import` and `test`

error: compilation failed: 3 error(s), 0 warning(s)
```

The first error is the real one; the others follow from the parser recovering.
Put the operator at the end of the line instead. Parentheses do not change
this rule: inside `( ... )` an operator must also follow its left operand on
the same line.

## Runtime errors

Type checking prevents most errors, but some can only happen at runtime:
integer overflow, division by zero, a failed conversion such as
`int("abc")`, or an operation on a `JSON` value of the wrong shape. An error
while evaluating:

- a step's `with`, `when`, `for`, `idempotency_key` or `compute` expression
  fails that step with the non-retryable code `EXPRESSION_ERROR`, which aborts
  the workflow and starts compensation,
- the workflow `return` fails the workflow with `EXPRESSION_ERROR`, without
  compensation,
- a query fails only that request, with `422 QUERY_FAILED`; the execution is
  not affected.

The error message names the expression and the cause, for example
`evaluating input: division by zero`.

## See also

- [Built-in functions](functions.md)
- [Compute steps](compute.md)
- [Types](types.md)
- [Conditions](conditions.md)
- [Grammar](grammar.md)
