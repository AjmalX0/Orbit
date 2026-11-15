# Schemas

A schema is a named record type. You declare schemas for data that several
contracts or workflows share, such as an order line or a payment request. This
page covers the schema syntax, required and optional fields, default values,
and the rules the compiler enforces on schema declarations.

## Declaring a schema

```orbit
schema LineItem {
    sku: String
    quantity: Int = 1          // default used when the field is absent
    note: String?              // optional: may be null or missing
}
```

A schema has a name (an identifier) and a list of fields. Each field has a
name, a type, and optionally a default value after `=`. Fields can be
separated by newlines, commas or semicolons.

Schemas can use any type, including other schemas and anonymous records:

```orbit
schema Address {
    line1: String
    city: String
    country: String = "US"
}

schema Customer {
    id: String
    email: String
    shipping: Address
    billing: Address?
    preferences: { newsletter: Bool = false, language: String = "en" }
}
```

A schema is a type, so you use its name anywhere a type is expected: in task
`input` and `output`, in workflow `input` and `output` fields, inside `List<>`
and `Map<String, >`, and in other schemas.

## Required, optional and defaulted fields

Every field is in one of three states:

| Declaration | Required in a value? | If absent |
|-------------|----------------------|-----------|
| `name: T` | yes | the value is invalid (`E0403` at compile time, a validation error at runtime) |
| `name: T?` | no | the field is absent; reading it gives `null` |
| `name: T = value` | no | the runtime inserts the default |

A field can be both optional and defaulted (`retries: Int? = 3`), in which
case the default is applied when the field is absent, and an explicit `null`
is kept.

The runtime applies defaults, recursively through nested records, lists and
maps, whenever it accepts a value of the type: execution input, task input
after `with` is evaluated, and worker output. Workers therefore always see the
defaulted fields, and a workflow can rely on a defaulted output field even if
an older worker does not send it.

## Default values

A default must be a constant expression of the field's type. It may use
literals, object and list literals, operators, `if` and the
[built-in functions](functions.md), but not `input`, `execution` or step
references. The compiler evaluates the default once and stores the result in
the compiled IR.

```orbit
schema RetryOptions {
    max_attempts: Int = 3
    window_ms: Int = 60 * 1000
    labels: Map<String, String> = { team: "payments" }
    tags: List<String> = []
    owner: String? = null
}
```

The compiler reports:

- a reference to `input`, `execution` or a step: `E0401`, for example
  ``error[E0401]: `input` cannot be used in a default value``;
- a value of the wrong type: `E0400`; for example `name: String = null` is
  rejected because `null` is not a `String` (declare `String?` instead);
- an expression that fails when evaluated, such as `1 / 0`: `E0205`.

## Rules the compiler enforces

| Code | Rule |
|------|------|
| `E0200` | A schema name is declared only once in the compilation. |
| `E0200` | A schema cannot be named after a built-in type (`String`, `Int`, `Float`, `Bool`, `Bytes`, `Duration`, `Timestamp`, `UUID`, `JSON`, `List`, `Map`). |
| `E0203` | A field name appears only once in a record. |
| `E0202` | Schemas cannot be recursive, directly or through other schemas, even through an optional field. |
| `E0201` | Every type a field uses must exist. |

Errors in schemas that no workflow uses are still reported.

A recursive schema is rejected with the cycle:

```orbit
schema Category {
    name: String
    parent: Category?
}
```

```text
error[E0202]: recursive schema `Category`
 --> category.orbit:3:13
  |
3 |     parent: Category?
  |             ^^^^^^^^
  = note: cycle: Category -> Category
  = help: ORBIT v0.1 schemas must be finite; break the cycle with a `JSON` or `String` id field

error: compilation failed: 1 error(s), 0 warning(s)
```

Model the relationship with an identifier instead, for example
`parent_id: String?`.

## Schemas are structural

Schemas are compared by their fields, not by their names. Two schemas with the
same fields are interchangeable, and a record with extra fields is accepted
where a schema with fewer fields is expected (see
[record width subtyping](types.md#record-width-subtyping)). The schema name
appears in diagnostics and in the compiled IR, where schemas are inlined.

## Anonymous records

Wherever a type is expected, you can write a record type inline instead of
naming it:

```orbit
task "risk.score" {
    version "1.0.0"
    input { customer_id: String, amount: Float }
    output { score: Float, reasons: List<String> }
}
```

Inline records follow the same rules as schemas, including defaults and
optional fields. Workflow `input` and `output` blocks are inline records too.

## See also

- [Types](types.md)
- [Task contracts](tasks.md)
- [Workflows](workflows.md)
- [Compiler diagnostics](../reference/diagnostics.md)
