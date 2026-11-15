# The ORBIT language (v0.1)

ORBIT is a declarative language for coordinating work done by programs written
in other languages. An ORBIT file declares **contracts** (what workers can do)
and **workflows** (what should happen, in which order, with which guarantees).
The compiler checks the whole thing (types, references, dependencies, policies)
before anything runs.

The formal grammar is in [`specification/grammar.ebnf`](../specification/grammar.ebnf).

## Files and imports

Source files use the `.orbit` extension. A file contains any number of
`schema`, `task` and `workflow` declarations, in any order. `import` brings in
another file, with the path resolved relative to the importing file:

```orbit
import "contracts.orbit"
```

Every file is loaded once, even if several files import it. All declarations
share one namespace.

Comments use `//` for a single line and `/* ... */` for blocks (blocks nest).

## Schemas

A schema is a named record type:

```orbit
schema LineItem {
    sku: String
    quantity: Int = 1          // default used when the field is absent
    note: String?              // optional: may be null or missing
}
```

Fields can be separated by newlines or commas.

## Types

| Type | Values |
|------|--------|
| `String`, `Int` (64-bit), `Float`, `Bool` | the usual |
| `Bytes` | base64 string |
| `Duration` | milliseconds; literals `500ms`, `30s`, `5m`, `2h`, `1d` |
| `Timestamp` | RFC 3339 string, e.g. `"2026-01-31T09:00:00Z"` |
| `UUID` | hyphenated UUID string |
| `JSON` | any value (dynamically checked) |
| `List<T>`, `Map<String, T>` | arrays and objects |
| `T?` | `T` or `null` |
| `{ a: Int, b: String }` | anonymous record |
| `Name` | a schema |

Rules of compatibility:

* `Int` widens to `Float`.
* A record is accepted where a record with fewer fields is expected (extra
  fields are allowed), except in object literals, where unknown fields are an
  error (they are almost always typos).
* `T` is accepted where `T?` is expected; **`T?` is not accepted where `T` is
  required**. Use `coalesce(value, default)`.
* `JSON` is accepted anywhere and accepts anything; the runtime validates
  payloads against contracts.

## Task contracts

A `task` declares the typed interface a worker implements. Workers in any
language register against it by name and version.

```orbit
task "payments.charge" {
    version "1.0.0"
    description "Charge a customer's card"
    input PaymentRequest                  // a schema...
    output { transaction_id: String }     // ...or an inline record
}
```

Task names starting with `orbit.` are reserved for built-in tasks. v0.1 ships
one built-in, `orbit.http.request` (see [HTTP adapter](#http-adapter)).

## Workflows

```orbit
workflow OrderProcessing {
    version "1.0.0"                 // required; definitions are immutable per version
    description "..."               // optional
    timeout 5m                      // optional overall deadline

    input  { order_id: String, amount: Float, currency: String = "USD" }
    output { status: String, transaction_id: String? }

    step ... { ... }

    return { status: "completed", transaction_id: process_payment.transaction_id }
}
```

`input` fields may have defaults, which the runtime applies when an execution
starts. Execution input is validated strictly: missing required fields, wrong
types and unknown fields are rejected before the execution is created.

## Steps

```orbit
step process_payment {
    worker "payments.charge"          // the task (or: workflow "Child", or: sleep 10s)
    using python                      // optional: only route to python workers
    after reserve_inventory           // explicit ordering
    when analyze_risk.score < 0.8     // run only if true; otherwise skipped
    with {                            // the task input, checked against the contract
        order_id: input.order_id
        amount: input.amount
        currency: input.currency
    }
    idempotency_key: "charge-" + input.order_id
    timeout 30s                       // per attempt
    queue_timeout 2m                  // fail if no worker picks it up in time
    retry {
        attempts: 5                   // total attempts, including the first
        backoff: exponential          // or fixed
        initial_delay: 500ms
        max_delay: 10s
        non_retryable: ["CARD_DECLINED"]
    }
    compensate {
        worker "payments.refund"
        with { transaction_id: process_payment.transaction_id, amount: input.amount }
    }
}
```

### Dependencies

A step runs once all of its dependencies have finished. Dependencies come from
two places:

1. **explicit** `after a, b`, and
2. **implicit data references**: reading `analyze_risk.score` in `with`,
   `when` or `idempotency_key` makes the step depend on `analyze_risk`.

The compiler builds the graph, rejects cycles (printing the cycle) and orders
steps deterministically. Steps without a dependency between them run
concurrently.

### Conditions and skipped steps

A step with `when` is skipped if the condition is false (or `null`). A skipped
step's output is `null`, so **references to a conditional step's output are
optional** (`process_payment.transaction_id` has type `String?`). Steps that
depend on a skipped step still run, and the type checker makes sure they handle
the missing value.

### Sleep steps

```orbit
step cool_off { sleep 24h }
step follow_up { worker "emails.send" after cool_off with { ... } }
```

`sleep` is a durable timer kept by the runtime. It survives restarts and needs
no worker.

### Fan-out: `for`

```orbit
step reserve {
    for item in input.items          // any List<T> expression
    max_parallel 4                   // optional concurrency limit
    worker "inventory.reserve"
    with { sku: item.sku, quantity: item.quantity }
    compensate {
        worker "inventory.release"
        with { reservation_id: reserve.reservation_id }   // this element's output
    }
}
```

A `for` step runs once per element. Each element is its own unit (`reserve[0]`,
`reserve[1]`, ...) with its own retries, timeouts and idempotency key
(`<execution>/reserve[i]`). The step's output is the **list** of element outputs,
in input order (`List<Out>`), and it succeeds when every element succeeds. If
an element fails permanently, the step fails; on compensation, every element
that *succeeded* is compensated individually.

The loop variable is in scope in `with`, `idempotency_key` and `compensate`.
It is not in scope in `when`, which decides once whether the whole step runs.
An empty list succeeds immediately with `[]`.

### Child workflows

```orbit
step ship {
    workflow "Shipping"              // defined in, or imported into, this compilation
    with { order_id: input.order_id, items: len(input.items) }
    timeout 1h
}
```

A `workflow` step starts another workflow as a durable **child execution** with
the deterministic id `<parent>.<step>` (`<parent>.<step>.<i>` inside a `for`), so
restarting after a crash never starts it twice. Its input and output are
type-checked against the child's `input`/`output`. Its result arrives when the
child completes. If the child fails, is cancelled or times out, the step fails
(`CHILD_CANCELLED`, `CHILD_TIMED_OUT`, or the child's own error code).
Cancelling or aborting the parent cancels its running children.

Child steps take no `using`, `retry`, `idempotency_key` or `queue_timeout`:
the child is durable on its own, so put retries on the child's steps.
Workflows that start each other in a cycle are rejected (`E0311`). The compiler
orders callees before callers, and `orbit deploy` / `orbit run` register them in
that order.

### Compensation

`compensate` declares the business action that undoes a step (Saga pattern).
If the workflow fails, is cancelled, or times out, the runtime waits for
in-flight steps to finish and then runs the compensation of every step that
**succeeded**, one at a time, in reverse order of completion. A compensation
can read its own step's output (non-optional there, because the step
succeeded) and the outputs of that step's ancestors. Compensation is explicit
business logic, not a database rollback.

## Expressions

Expressions are pure and deterministic: no I/O and no clock. That makes every
decision replayable from history.

| Form | Example |
|------|---------|
| literals | `"text"`, `42`, `3.14`, `true`, `null`, `30s` |
| workflow input | `input.order_id` |
| step output | `analyze_risk.score`, `charge` (the whole record) |
| loop variable | `item.sku` inside a `for` step |
| execution metadata | `execution.id`, `execution.workflow`, `execution.version` |
| objects and lists | `{ id: input.id, tags: ["a", "b"] }` |
| indexing | `input.items[0]`, `headers["content-type"]` (optional result) |
| arithmetic | `+ - * / %` (`+` also concatenates strings and lists) |
| comparison | `== != < <= > >=` |
| logic | `&& || !` (short-circuit) |
| conditional | `if risk.score > 0.8 then "review" else "ok"` |

Member access propagates `null`: if `a` is `null`, `a.b` is `null`.

An infix operator must be on the same line as its left operand, so that
`{ a: 1` newline `b: -2 }` reads as two fields.

### Built-in functions

| Function | Result |
|----------|--------|
| `len(x)` | length of a String, List or Map |
| `coalesce(a, b, ...)` | first non-null argument (non-optional if any argument is) |
| `is_null(x)` | `Bool` |
| `string(x)`, `int(x)`, `float(x)` | conversions |
| `contains(haystack, needle)` | substring, list membership or map key |
| `lower(s)`, `upper(s)`, `trim(s)` | string helpers |
| `join(list, sep)` | `String` |
| `keys(map)` | `List<String>` |
| `min(...)`, `max(...)`, `abs(x)` | numbers |

## Return value

`return` maps step results to the declared `output`. It is type-checked against
`output`, and the runtime validates the result again before completing the
execution. If a workflow declares no `output`, its output type is inferred from
`return`.

## HTTP adapter

`orbit.http.request` calls an HTTP endpoint from the runtime without a worker.
It must be enabled on the server with `--enable-http-adapter`.

```orbit
step fetch {
    worker "orbit.http.request"
    with { method: "POST", url: "https://api.example.com/hooks", body: { id: input.id } }
    timeout 10s
    retry { attempts: 3, backoff: exponential }
}
// fetch.status: Int, fetch.headers: Map<String, String>, fetch.body: JSON
```

`5xx` and `429` responses are retryable failures. Other statuses are returned
to the workflow.

## Tooling

- `orbit check` / `orbit compile`: diagnostics and IR. `--format json` gives
  machine-readable diagnostics.
- `orbit fmt [--check] files...`: formats indentation and spacing, and keeps your
  comments and line breaks. It verifies that its output lexes to the same token
  stream before writing, so formatting can never change meaning.
- `orbit lsp`: the language server behind the VS Code extension
  (`editors/vscode`). It provides diagnostics as you type, hover,
  go to definition, completion, outline and formatting.
- `orbit graph`: Mermaid or Graphviz view of the step graph.

## Versioning

* Each workflow must declare a `version`. A registered `name@version` can never
  change: registering different content under the same version is rejected.
* Running executions stay on the definition they started with.
* Task contracts are versioned on their own. A worker only receives tasks
  whose `task_name@contract_version` it announced.
* The IR has its own version (`orbit-ir/1`), and the runtime rejects versions
  it does not support.
EOF