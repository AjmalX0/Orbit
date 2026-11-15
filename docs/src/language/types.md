# Types

ORBIT is statically typed. The compiler checks every value that flows between
the workflow input, task contracts, step outputs, child workflows and the
workflow result before anything runs, and the runtime validates payloads again
at every boundary. This page lists every type, its JSON encoding, how to write
values of it, and the compatibility rules the type checker applies.

## Primitive types

| Type | Values | JSON encoding | How to write a value |
|------|--------|---------------|----------------------|
| `String` | Unicode text | string | `"text"` |
| `Int` | signed 64-bit integer | integer | `42`, `-7`, `1_000_000` |
| `Float` | 64-bit floating point | number (integers accepted) | `3.14`, `-0.5`; an `Int` also converts |
| `Bool` | `true` or `false` | boolean | `true`, `false` |
| `Bytes` | binary data | base64 string (standard alphabet, padded) | no literal; comes from input or step output |
| `Duration` | a length of time | non-negative integer, in milliseconds | `500ms`, `30s`, `1.5m`, `2h`, `1d` |
| `Timestamp` | a point in time | RFC 3339 string | a string literal such as `"2026-01-31T09:00:00Z"` |
| `UUID` | a UUID | hyphenated string, `8-4-4-4-12` hex digits | a string literal such as `"123e4567-e89b-12d3-a456-426614174000"` |
| `JSON` | any JSON value | any | any expression |

Details:

- **Int.** Integer literals that do not fit in 64 bits are `E0006`. Arithmetic
  that overflows at runtime is an expression error, not a wrap-around.
- **Duration.** A duration literal is a number followed by a unit: `ms`, `s`,
  `m` (minutes), `h` or `d`. Fractional values are allowed and rounded to the
  nearest millisecond (`1.5s` is 1500 ms). A bare number where a duration is
  required is `E0101`, and an unknown unit is `E0005`. `Duration` supports no
  operators: you cannot add or compare durations.
- **Timestamp.** The accepted form is
  `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`. The `T` may also be `t` or a
  space, and `Z` may be `z`. A string literal checked against `Timestamp` is
  validated at compile time (`E0400`).
- **UUID.** 36 characters, hyphens at positions 9, 14, 19 and 24, hexadecimal
  digits in either case elsewhere. String literals checked against `UUID` are
  validated at compile time (`E0400`).
- **Bytes.** A string literal is a `String`, not `Bytes`, so passing
  `"aGVsbG8="` where `Bytes` is required is `E0400`. Pass `Bytes` values
  through from the workflow input or a step output, or declare the field as
  `String` if you build the encoding yourself.
- **String to UUID or Timestamp.** Only literals are converted. A `String`
  value read from input or a step output is not accepted where `UUID` or
  `Timestamp` is required; declare the source field with the precise type.

## Composite types

| Type | Values | JSON encoding |
|------|--------|---------------|
| `List<T>` | ordered list of `T` | array |
| `Map<String, T>` | string keys to `T` values | object |
| `T?` | a `T` or `null` | `null` or the encoding of `T` |
| `{ a: Int, b: String }` | anonymous record | object |
| `Name` | the record declared by `schema Name` | object |

- Map keys must be `String`, because maps are JSON objects. `Map<Int, String>`
  is `E0204`.
- `T??` is the same type as `T?`.
- Record types, named or anonymous, list fields with optional defaults. See
  [Schemas](schemas.md).
- Type names are case-sensitive. An unknown type name is `E0201`, with a
  suggestion when a known type is close. `List` and `Map` without type
  arguments are `E0201` too.

```orbit
schema Shipment {
    id: UUID
    created_at: Timestamp
    packages: List<{ sku: String, weight_kg: Float }>
    labels: Map<String, String>
    note: String?
    raw: JSON
}
```

## Optional types and null

`T?` means "a `T`, or `null`". Three things produce optional values:

- fields declared `T?`,
- references to a step that has a `when` condition, because the step may be
  skipped (see [Conditions](conditions.md)),
- indexing a list or map, because the element may be missing.

The literal `null` has its own type, written `null` in diagnostics. It is
accepted only where an optional type (or `JSON`) is expected.

A value of type `T?` is not accepted where `T` is required. The compiler
reports `E0405` and suggests `coalesce`:

```orbit
task "users.notify" {
    input { name: String }
    output { ok: Bool }
}

workflow Notify {
    version "1.0.0"
    input { name: String? }
    step send { worker "users.notify" with { name: input.name } }
}
```

```text
error[E0405]: value may be `null` but `String` is required
 --> notify.orbit:9:52
  |
9 |     step send { worker "users.notify" with { name: input.name } }
  |                                                    ^^^^^^^^^^
  = help: provide a fallback with `coalesce(value, default)`, or declare the target as optional with `?`

error: compilation failed: 1 error(s), 0 warning(s)
```

Fix it with a fallback, `name: coalesce(input.name, "customer")`, or make the
contract field optional.

## The JSON type

`JSON` is the dynamic type. Any value is accepted where `JSON` is expected, and
a `JSON` value is accepted wherever any type is expected. Member access and
indexing on `JSON` produce `JSON`, and operators that involve a `JSON`
operand are not checked at compile time. The runtime still validates payloads
against contracts, so a wrong value surfaces as `INPUT_CONTRACT_VIOLATION` or
`OUTPUT_CONTRACT_VIOLATION` when the step runs.

`JSON` counts as optional: it may hold `null`.

## Compatibility rules

The type checker asks one question everywhere: can a value of type *S* be used
where type *D* is expected? The rules are:

| Rule | Example |
|------|---------|
| Every type is compatible with itself. | `String` → `String` |
| `JSON` is compatible in both directions. | `Int` → `JSON`, `JSON` → `Int` |
| `null` is compatible with any optional type. | `null` → `String?` |
| `T` is compatible with `T?`. | `Int` → `Int?` |
| `T?` is **not** compatible with `T` (`E0405`). | `Int?` → `Int` is rejected |
| `Int` widens to `Float`. The reverse is rejected. | `Int` → `Float` |
| Lists and maps are compatible when their element types are. | `List<Int>` → `List<Float>` |
| A record is compatible with a record type when every field the target requires is present and compatible. | see below |

A mismatch is `E0400`, with a note that names the field or element at fault
when the types are nested.

### Record width subtyping

Records are structural: the schema name does not matter, only the fields. A
record with *more* fields than the target is accepted. A field the target
declares as optional (`T?`) or with a default may be missing from the source.
This lets you pass a whole step output or input record to a contract that only
needs part of it:

```orbit
task "crm.lookup" {
    input { customer: { id: String } }
    output { tier: String }
}

workflow Lookup {
    version "1.0.0"
    input { customer: { id: String, email: String, name: String } }
    step lookup { worker "crm.lookup" with { customer: input.customer } }
}
```

### Object literals are exact

An object literal written directly where a record type is expected must not
contain fields that the record does not declare. Extra fields there are almost
always typos, so the compiler reports `E0404` and suggests the closest field:

```orbit
task "crm.lookup" {
    input { customer_id: String }
    output { tier: String }
}

workflow Lookup {
    version "1.0.0"
    input { id: String }
    step lookup { worker "crm.lookup" with { customer_id: input.id, cutomer_tier: "gold" } }
}
```

```text
error[E0404]: unknown field `cutomer_tier` for `{ customer_id: String }`
 --> lookup.orbit:9:69
  |
9 |     step lookup { worker "crm.lookup" with { customer_id: input.id, cutomer_tier: "gold" } }
  |                                                                     ^^^^^^^^^^^^
  = help: did you mean `customer_id`?

error: compilation failed: 1 error(s), 0 warning(s)
```

Missing required fields in an object literal are `E0403`, which lists every
missing field with its type.

## Type inference

Expressions without an expected type are inferred:

| Expression | Inferred type |
|------------|---------------|
| `{ a: 1, b: "x" }` | `{ a: Int, b: String }` |
| `{ a: null }` | `{ a: JSON }` (a `null` field is typed `JSON`) |
| `[1, 2.5]` | `List<Float>` |
| `[]` | `List<JSON>` |
| `[1, "a"]` | `E0400`: list items have incompatible types |
| `if c then "a" else null` | `String?` |
| `if c then 1 else 2.5` | `Float` |

When two types have to be combined (list items, `if` branches, `coalesce`
arguments), the compiler uses the smallest type that both fit into: equal
types stay the same, `null` with `T` gives `T?`, `Int` with `Float` gives
`Float`, `JSON` with anything gives `JSON`, and of two compatible records the
one with fewer fields wins. Other combinations are `E0400`.

A workflow without an `output` declaration takes the inferred type of its
`return` expression as its output type. See [Workflows](workflows.md#output-and-return).

## Runtime validation

Values cross process boundaries as JSON, so the runtime validates them against
the declared types:

| Boundary | Defaults applied | Unknown fields | Failure |
|----------|------------------|----------------|---------|
| Execution input (start request) | yes | rejected | request fails with `INVALID_INPUT` |
| Child workflow input | yes | allowed | step fails with `CHILD_START_FAILED` |
| Task input, after evaluating `with` | yes | allowed | step fails with `INPUT_CONTRACT_VIOLATION` |
| Worker output | yes | allowed | attempt fails with `OUTPUT_CONTRACT_VIOLATION` (not retried) |
| Workflow result, after evaluating `return` | no | allowed | workflow fails with `OUTPUT_CONTRACT_VIOLATION` |

A worker that returns `null` as its output is treated as returning `{}`.
Validation of `Float` accepts any JSON number. Validation of `Int` requires an
integer, and validation of `Duration` requires a non-negative integer.

## See also

- [Schemas](schemas.md)
- [Expressions](expressions.md)
- [Built-in functions](functions.md)
- [Conditions](conditions.md)
- [Compiler diagnostics](../reference/diagnostics.md)
