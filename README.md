# ORBIT

**One workflow. Every language. Reliable execution.**

ORBIT is a declarative programming language and durable runtime for
orchestrating work across services written in different languages. You
describe *what* should happen in a `.orbit` file. The compiler type-checks
every step against its worker's contract, and the runtime executes it durably:
retries, timeouts, compensation, crash recovery and a complete audit history.
Your business logic stays in Python, TypeScript, Go or anything that can speak
HTTP.

```orbit
import "contracts.orbit"

workflow OrderProcessing {
    version "1.0.0"
    input  { order_id: String, customer_id: String, amount: Float, items: List<LineItem> }
    output { status: String, transaction_id: String? }

    step validate_order {
        worker "orders.validate"
        using typescript
        with { order_id: input.order_id, amount: input.amount, items: input.items }
        timeout 10s
    }

    step analyze_risk {                         // runs in parallel with validate_order
        worker "risk.analyze"
        using python
        with { customer_id: input.customer_id, amount: input.amount }
        retry { attempts: 3, backoff: exponential, initial_delay: 1s }
    }

    step process_payment {
        worker "payments.charge"
        using python
        after validate_order
        when analyze_risk.score < 0.8           // implicit dependency on analyze_risk
        with { order_id: input.order_id, amount: input.amount, currency: "USD" }
        idempotency_key: "charge-" + input.order_id
        compensate {
            worker "payments.refund"
            with { transaction_id: process_payment.transaction_id, amount: input.amount }
        }
    }

    return {
        status: if process_payment.transaction_id == null then "held_for_review" else "completed"
        transaction_id: process_payment.transaction_id   // String? — the step is conditional
    }
}
```

Mistakes are caught before anything runs. For example, a later step that uses
the payment's transaction id as a required `String`, forgetting that the
payment can be skipped:

```text
error[E0405]: value may be `null` but `String` is required
  --> order.orbit:37:57
   |
37 |         with { customer_id: input.customer_id, message: process_payment.transaction_id }
   |                                                         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = help: provide a fallback with `coalesce(value, default)`, or declare the target as optional with `?`
```

## Quick start

Requirements: Rust 1.80+, plus Python 3.9+ and/or Node.js 22.18+ for the
example workers.

```bash
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

```bash
orbit server --db orbit.db &
```

```bash
PYTHONPATH=sdks/python python3 examples/workers/python/order_workers.py &
node examples/workers/typescript/order_workers.ts &
```

```bash
orbit run examples/order_processing.orbit --input '{
  "order_id": "A-1", "customer_id": "c-42", "amount": 120.0,
  "items": [{"sku": "BOOK-1", "quantity": 2}]
}'
```

```text
deployed OrderProcessing@1.0.0 (sha256:4fdfac082bf3)
started execution 385521e6-fe5c-4c95-9734-4fe3e46bc1bb
  ✓ validate_order                 succeeded 8ms
  ✓ analyze_risk                   succeeded 11ms
  ✓ reserve_inventory              succeeded 6ms
  ✓ process_payment                succeeded 5ms
  ✓ send_notification              succeeded 7ms

COMPLETED 385521e6-fe5c-4c95-9734-4fe3e46bc1bb  OrderProcessing@1.0.0
  elapsed: 40ms
{
  "status": "completed",
  "transaction_id": "tx-A-1",
  "risk_score": 0.101
}
```

Try `"amount": 9000`. The card is declined, so ORBIT compensates the
inventory reservation that already succeeded and records every decision
(`orbit logs <id>`):

```text
+    15ms  #13  StepDispatched            step=process_payment attempt=1 worker_id=orders-python
+    21ms  #14  StepAttemptFailed         step=process_payment attempt=1 error=CARD_DECLINED charge of 9000 USD declined
+    21ms  #15  StepFailed                step=process_payment error=CARD_DECLINED charge of 9000 USD declined
+    21ms  #16  StepCancelled             step=send_notification
+    21ms  #17  CompensationStarted       reason=step `process_payment` failed: charge of 9000 USD declined
+    21ms  #18  StepScheduled             step=compensate:reserve_inventory attempt=1
+    23ms  #19  StepDispatched            step=compensate:reserve_inventory attempt=1 worker_id=orders-typescript
+    28ms  #20  StepSucceeded             step=compensate:reserve_inventory attempt=1
+    28ms  #21  CompensationCompleted
+    28ms  #22  WorkflowFailed            error=CARD_DECLINED step `process_payment` failed: charge of 9000 USD declined
```

With `"customer_id": "risky-1"` the payment is skipped by `when` and the order
is held for review. Other commands:

```bash
orbit ps
orbit status <execution-id>
orbit workers list
orbit graph examples/order_processing.orbit
```

Or start a new project with `orbit init my-project`.

## What's in the box

| Component | Path | |
|---|---|---|
| Language front end | `crates/orbit-syntax` | lexer, parser with error recovery, AST, rustc-style diagnostics |
| Compiler | `crates/orbit-compiler` | imports, name resolution, structural type checker, dependency graph, cycle detection, IR lowering |
| IR | `crates/orbit-ir` | `orbit-ir/1`: types, payload validation, pure expression evaluator, invariants, SHA-256 definition hash |
| Runtime | `crates/orbit-runtime` | event-sourced engine with a pure decider, SQLite, worker registry, dispatch, retries, timeouts, compensation, recovery, HTTP API, metrics |
| CLI | `crates/orbit-cli` | `init check validate compile graph server deploy run status inspect logs cancel ps workers` |
| Python SDK | `sdks/python` | standard library only |
| TypeScript SDK | `sdks/typescript` | zero dependencies, runs on Node.js and anything with `fetch` |
| Worker protocol | `protocol/` | canonical `.proto`, plus the HTTP/JSON binding spec |
| Specification | `specification/` | RFC-0001 with implementation record, EBNF grammar |
| Docs | `docs/` | [language](docs/language.md), [runtime semantics](docs/runtime.md), [diagnostics](docs/diagnostics.md) |
| Editor support | `editors/vscode` | syntax highlighting for `.orbit` |

## Guarantees, precisely

* **Durable.** Every state change is an event committed to SQLite (WAL,
  `synchronous=FULL`) before it takes effect. State is rebuilt from history on
  restart.
* **At-least-once delivery, never "exactly-once".** Every task carries an
  idempotency key that is stable across retries and redeliveries. If a worker
  or the runtime crashes mid-task, the outcome counts as *unknown, possibly
  done*. The same attempt is redelivered, and a late result from the original
  delivery is still accepted (reconciliation).
* **Immutable definitions.** `name@version` is bound to a content hash.
  Running executions keep the definition they started with.
* **Contract safety at both ends.** The compiler checks every step's input and
  every reference. The runtime validates execution inputs and worker outputs
  against the same types.

Details: [docs/runtime.md](docs/runtime.md).

## Testing

```bash
cargo test
```

```bash
cargo build && python3 -I tests/e2e/test_e2e.py
```

`cargo test` runs the compiler, IR and runtime suites. The second command is
the end-to-end suite: a real server, real Python and TypeScript workers, and a
SIGKILL crash-recovery scenario. Acceptance criteria are mapped to tests in
[RFC-0001 §15](specification/RFC-0001.md#15-testing-strategy-and-acceptance-criteria).

## Status

v0.1: single-node, SQLite, HTTP/JSON worker protocol, Python and TypeScript
SDKs. Next: gRPC binding, PostgreSQL, OpenTelemetry, `orbit fmt` and an LSP,
fan-out (`for each`), child workflows, then multi-node execution. See
[RFC-0001 Appendix A](specification/RFC-0001.md#appendix-a--v010-implementation-record).
