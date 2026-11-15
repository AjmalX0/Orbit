# Built-in functions

ORBIT provides a fixed set of pure functions for mapping data between steps:
null handling, conversions, string and collection helpers, numeric helpers,
and collection functions that take a lambda. Like all expressions, they are deterministic and perform no I/O.
This page lists every function with its signature, argument and result types,
behavior with `null`, runtime errors, and examples.

## Summary

| Function | Signature | Result |
|----------|-----------|--------|
| [`coalesce`](#coalesce) | `coalesce(a, b, ...)` | the first non-null argument |
| [`is_null`](#is_null) | `is_null(x)` | `Bool` |
| [`len`](#len) | `len(String \| List<T> \| Map<String, T>)` | `Int` |
| [`string`](#string) | `string(x)` | `String` |
| [`int`](#int) | `int(Int \| Float \| String \| Bool)` | `Int` |
| [`float`](#float) | `float(Int \| Float \| String \| Bool)` | `Float` |
| [`contains`](#contains) | `contains(haystack, needle)` | `Bool` |
| [`lower`](#lower-upper-trim) | `lower(String)` | `String` |
| [`upper`](#lower-upper-trim) | `upper(String)` | `String` |
| [`trim`](#lower-upper-trim) | `trim(String)` | `String` |
| [`join`](#join) | `join(List<T>, String)` | `String` |
| [`keys`](#keys) | `keys(Map<String, T>)` | `List<String>` |
| [`min`](#min-max) | `min(number, ...)` | number |
| [`max`](#min-max) | `max(number, ...)` | number |
| [`abs`](#abs) | `abs(number)` | number |
| [`sum`](#sum) | `sum(List<Int> \| List<Float>)` | `Int` or `Float` |
| [`map`](#map) | `map(List<T>, T => U)` | `List<U>` |
| [`filter`](#filter) | `filter(List<T>, T => Bool)` | `List<T>` |
| [`find`](#find) | `find(List<T>, T => Bool)` | `T?` |
| [`any`](#any-all) | `any(List<T>, T => Bool)` | `Bool` |
| [`all`](#any-all) | `all(List<T>, T => Bool)` | `Bool` |

General rules:

- Function names are case-sensitive, and there are no user-defined functions.
- An unknown name or a wrong number of arguments is `E0407`. The diagnostic
  suggests a close name or prints the signature.
- An argument of the wrong type is `E0400`
  (``argument 1 of `len` must be a String, List or Map, found `Int` ``).
- Arguments of type `JSON` are accepted everywhere and checked at runtime.
- A runtime error, such as `int("abc")`, fails the step being scheduled (or
  the workflow, in `return`) with `EXPRESSION_ERROR`. See
  [Expressions](expressions.md#runtime-errors).

## coalesce

```text
coalesce(a: T?, b: T?, ..., z: T) -> T
```

Returns the first argument that is not `null`, or `null` if all are `null`.
Takes one or more arguments.

| Aspect | Behavior |
|--------|----------|
| Argument types | Must be compatible with each other (`E0400` otherwise). `Int` and `Float` combine to `Float`. |
| Result type | The combined type, **non-optional if any argument is non-optional**, optional otherwise. |
| Evaluation | All arguments are evaluated. |

```orbit
workflow Coalesce {
    version "1.0.0"
    input { nickname: String?, name: String?, discount: Float? }
    return {
        display: coalesce(input.nickname, input.name, "customer")   // String
        maybe: coalesce(input.nickname, input.name)                 // String?
        discount: coalesce(input.discount, 0)                       // Float
    }
}
```

`coalesce` is the standard way to turn an optional value, such as a field of a
skipped step, into a required one. See [Conditions](conditions.md).

## is_null

```text
is_null(x: T) -> Bool
```

Returns `true` if `x` is `null`. Accepts any type. `is_null(x)` is equivalent
to `x == null`.

```orbit
workflow IsNull {
    version "1.0.0"
    input { coupon: String? }
    return { has_coupon: !is_null(input.coupon) }
}
```

## len

```text
len(x: String | List<T> | Map<String, T>) -> Int
```

| Argument | Result |
|----------|--------|
| `String` | the number of Unicode characters (code points), not bytes: `len("héllo")` is `5` |
| `List<T>` | the number of items |
| `Map<String, T>` | the number of keys |
| `null` | `0` |

Optional arguments are accepted, and `null` counts as empty. Other types are
`E0400`.

```orbit
workflow Len {
    version "1.0.0"
    input { items: List<LineItem>, note: String? }
    return { item_count: len(input.items), note_length: len(input.note) }
}
```

## string

```text
string(x: T) -> String
```

Converts any value to a string:

| Argument | Result |
|----------|--------|
| a `String` | unchanged |
| `null` | `"null"` |
| a number | its JSON form: `string(42)` is `"42"`, `string(1.5)` is `"1.5"` |
| a `Bool` | `"true"` or `"false"` |
| a list, map or record | compact JSON: `string([1, 2])` is `"[1,2]"`, `string({ a: 1 })` is `"{\"a\":1}"` |

```orbit
workflow ToString {
    version "1.0.0"
    input { order_id: String, attempt: Int }
    return { key: input.order_id + "#" + string(input.attempt) }
}
```

## int

```text
int(x: Int | Float | String | Bool) -> Int
```

| Argument | Result |
|----------|--------|
| `Int` | unchanged |
| `Float` | truncated toward zero: `int(2.9)` is `2`, `int(-2.9)` is `-2` |
| `String` | parsed as a base-10 integer after trimming whitespace: `int(" 42 ")` is `42`; `int("1.5")` and `int("abc")` are runtime errors |
| `Bool` | `1` for `true`, `0` for `false` |
| `null` | runtime error |

The compiler accepts optional arguments, but a `null` value fails at runtime.
Use `int(coalesce(x, 0))` when the argument may be `null`.

## float

```text
float(x: Int | Float | String | Bool) -> Float
```

| Argument | Result |
|----------|--------|
| `Int` or `Float` | the value as a float: `float(5)` is `5.0` |
| `String` | parsed after trimming whitespace: `float("2.5")` is `2.5`; an invalid string is a runtime error |
| `Bool` | `1.0` for `true`, `0.0` for `false` |
| `null` | runtime error |

Use `float` to force float arithmetic on a value that may arrive as a JSON
integer: `float(input.price) / 2`. See
[Arithmetic](expressions.md#arithmetic).

Before ORBIT 0.2, the compiler accepted a `Bool` argument but the runtime
rejected it with `EXPRESSION_ERROR`.

## contains

```text
contains(haystack: String, needle: String) -> Bool
contains(haystack: List<T>, needle: T) -> Bool
contains(haystack: Map<String, T>, key: String) -> Bool
```

| Haystack | Result |
|----------|--------|
| `String` | `true` if `needle` is a substring; `contains("hello", "ell")` is `true` |
| `List<T>` | `true` if an item equals `needle`, using structural equality (`contains([1, 2], 2.0)` is `true`) |
| `Map<String, T>` or a record | `true` if the key exists |
| `null` | `false` |

The compiler checks only the number of arguments (2), not their types. A
combination outside the table, such as a string haystack with a number needle,
is a runtime error.

```orbit
workflow Contains {
    version "1.0.0"
    input { tags: List<String>, email: String }
    return {
        vip: contains(input.tags, "vip")
        corporate: contains(lower(input.email), "@example.com")
    }
}
```

## lower, upper, trim

```text
lower(s: String) -> String
upper(s: String) -> String
trim(s: String) -> String
```

| Function | Result |
|----------|--------|
| `lower` | Unicode lowercase: `lower("ÀB")` is `"àb"` |
| `upper` | Unicode uppercase |
| `trim` | the string without leading and trailing Unicode whitespace |

With a `String?` argument the result is `String?`, and `null` returns `null`.
Other argument types are `E0400`.

```orbit
workflow Normalize {
    version "1.0.0"
    input { email: String, nickname: String? }
    return {
        email: lower(trim(input.email))          // String
        nickname: upper(input.nickname)          // String?
    }
}
```

## join

```text
join(items: List<T>, separator: String) -> String
```

Concatenates the items, with `separator` between them. String items are used
as they are; other items are converted to their JSON form, as with
[`string`](#string): `join([1, 2], ",")` is `"1,2"`. An empty list gives `""`.

The compiler accepts an optional list, but a `null` list is a runtime error.

```orbit
workflow Join {
    version "1.0.0"
    input { skus: List<String> }
    return { summary: join(input.skus, ", ") }
}
```

## keys

```text
keys(m: Map<String, T>) -> List<String>
```

Returns the keys of a map (or the field names of a record), in the order they
appear in the JSON object. A `null` argument is a runtime error.

```orbit
workflow Keys {
    version "1.0.0"
    input { labels: Map<String, String> }
    return { label_names: keys(input.labels) }
}
```

## min, max

```text
min(a: number, b: number, ...) -> number
max(a: number, b: number, ...) -> number
```

Return the smallest or largest argument. Take one or more arguments, each
`Int` or `Float` (or `JSON`). Optional arguments are `E0400`; wrap them in
`coalesce`.

| Aspect | Behavior |
|--------|----------|
| Result type | `Int` if every argument is `Int`, otherwise `Float`. |
| Result value | The winning argument itself, unchanged: `max(3, 2.5)` returns `3`. |
| Ties | The first of the equal arguments wins. |

```orbit
workflow Clamp {
    version "1.0.0"
    input { requested: Int }
    return { batch_size: max(1, min(input.requested, 500)) }
}
```

## abs

```text
abs(x: Int) -> Int
abs(x: Float) -> Float
```

Returns the absolute value. The result type is the argument type. An optional
argument is `E0400`. `abs` of the minimum `Int` overflows and is a runtime
error.

```orbit
workflow Abs {
    version "1.0.0"
    input { expected: Float, actual: Float }
    return { drift: abs(input.actual - input.expected) }
}
```

## sum

```text
sum(list: List<Int>) -> Int
sum(list: List<Float>) -> Float
```

Adds the elements of a list of numbers. The result is `Int` for a
`List<Int>` and `Float` otherwise. The list may be optional; `sum` of `null`
and of an empty list is `0`. An element that is not a number is a runtime
error (possible only with `JSON` values), and so is `Int` overflow. Any other
argument type is `E0400`
(``argument 1 of `sum` must be a List of Int or Float, found `List<String>` ``).

At runtime, the result is an integer whenever every element is an integer
JSON number, so the sum of an empty `List<Float>` is the JSON number `0`, not
`0.0`. JSON consumers that distinguish the two should not rely on the
representation.

```orbit
workflow Invoice {
    version "1.0.0"
    input { quantities: List<Int>, prices: List<Float> }
    return { units: sum(input.quantities), total: sum(input.prices) }
}
```

## Collection functions

`map`, `filter`, `find`, `any` and `all` take a list and a lambda: a
one-parameter function written `parameter => expression`. The function calls
the lambda once per element, in list order, with the parameter bound to the
element. See [Expressions](expressions.md#lambdas) for the lambda syntax.

Rules shared by all five:

- They take exactly two arguments (`E0407` otherwise, with the signature in a
  note). The first must be a list (`E0400`:
  ``argument 1 of `map` must be a List, found `String` ``); a `List<T>?` is
  accepted, and `null` behaves like an empty list. The second must be a
  lambda (`E0400`: ``argument 2 of `any` must be a lambda``).
- The lambda body can read `input`, `execution`, step outputs, loop variables
  and the parameters of enclosing lambdas. Reading a step's output adds a
  dependency, as anywhere else.
- For `filter`, `find`, `any` and `all`, the body must have type `Bool`
  (`E0400`, or `E0405` for a `Bool?`). At runtime a `null` condition counts as
  false.
- The parameter must not reuse the name of a step, a loop variable, an
  enclosing lambda parameter, `input` or `execution` (`E0413`).
- A lambda anywhere else is `E0415`.
- An error inside the body, such as a division by zero for one element,
  fails the whole call.

### map

```text
map(list: List<T>, f: T => U) -> List<U>
```

Returns the list of `f(x)` for each element `x`.

```orbit
workflow Emails {
    version "1.0.0"
    input { users: List<{ email: String, name: String }> }
    return { emails: map(input.users, u => lower(u.email)) }
}
```

### filter

```text
filter(list: List<T>, f: T => Bool) -> List<T>
```

Returns the elements for which `f` is true, in their original order.

```orbit
workflow Restock {
    version "1.0.0"
    input { items: List<{ sku: String, stock: Int }> }
    return { low: map(filter(input.items, i => i.stock < 5), i => i.sku) }
}
```

### find

```text
find(list: List<T>, f: T => Bool) -> T?
```

Returns the first element for which `f` is true, or `null` when there is
none. The result is always optional.

```orbit
workflow Primary {
    version "1.0.0"
    input { addresses: List<{ city: String, primary: Bool }> }
    return { city: coalesce(find(input.addresses, a => a.primary).city, "unknown") }
}
```

### any, all

```text
any(list: List<T>, f: T => Bool) -> Bool
all(list: List<T>, f: T => Bool) -> Bool
```

`any` is true when `f` is true for at least one element, and `all` when it is
true for every element. Both stop at the first element that decides the
result. For an empty list, `any` is `false` and `all` is `true`.

```orbit
task "mail.send" {
    input { to: List<String> }
    output { sent: Int }
}

workflow Notify {
    version "1.0.0"
    input { users: List<{ email: String, active: Bool }> }

    step send {
        worker "mail.send"
        when any(input.users, u => u.active)
        with { to: map(filter(input.users, u => u.active), u => u.email) }
    }
}
```

A workflow that uses a lambda anywhere compiles to `orbit-ir/2`. See
[Versioning](versioning.md#ir-version).

## See also

- [Expressions](expressions.md)
- [Compute steps](compute.md)
- [Types](types.md)
- [Conditions](conditions.md)
