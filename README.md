# ORBIT

**One workflow. Every language. Reliable execution.**

ORBIT is a declarative programming language and durable runtime for
orchestrating work across services written in different languages. You
describe *what* should happen in a `.orbit` file. The compiler type-checks
every step against its worker's contract, and the runtime executes it durably:
retries, timeouts, compensation, crash recovery and a complete audit history.
Your business logic stays in Python, TypeScript, Go, Rust, Java, C# or anything
that can speak HTTP.

**[Website](https://ajmalx0.github.io/Orbit/)** ·
**[Documentation](https://ajmalx0.github.io/Orbit/docs/)** ·
**[Quick start](https://ajmalx0.github.io/Orbit/docs/getting-started/quickstart.html)** ·
**[Releases](https://github.com/AjmalX0/Orbit/releases/latest)**

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

## Install

**Linux** (x86_64, arm64), with checksum verification:

```bash
curl -fsSL https://github.com/AjmalX0/Orbit/releases/latest/download/install.sh | sh
```

**macOS, or any platform with Rust 1.89+:**

```bash
cargo install orbit-lang
```

**Windows:** download `orbit-x86_64-pc-windows-msvc.zip` from
[GitHub Releases](https://github.com/AjmalX0/Orbit/releases/latest).

**Container:**

```bash
docker run -p 7233:7233 -e ORBIT_TOKEN=change-me -v orbit-data:/data ghcr.io/ajmalx0/orbit:latest
```

See [Installation](https://ajmalx0.github.io/Orbit/docs/getting-started/installation.html)
for every option, including upgrading and building from source.

| | Package |
|---|---|
| CLI / runtime | `cargo install orbit-lang`, the install script, GitHub Releases, `ghcr.io/ajmalx0/orbit` |
| Python | `pip install orbit-lang-sdk` |
| TypeScript / JavaScript | `npm install orbit-lang` |
| Go | `go get github.com/AjmalX0/Orbit/sdks/go` |
| Rust | `cargo add orbit-worker` |
| Java | `io.github.ajmalx0:orbit-sdk` (Maven Central) |
| C# / .NET | `dotnet add package OrbitLang.Sdk` |
| VS Code | `orbit-lang.vsix` from [GitHub Releases](https://github.com/AjmalX0/Orbit/releases/latest), then `code --install-extension orbit-lang.vsix` |

## Quick start

Run the order example with Python and TypeScript workers, from a clone:

```bash
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

```bash
orbit server &
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

More examples: [`examples/approval.orbit`](examples/approval.orbit) (signals,
queries and compute steps), [`examples/fulfilment.orbit`](examples/fulfilment.orbit) (fan-out
and a child workflow), [`examples/polyglot.orbit`](examples/polyglot.orbit)
(Python, TypeScript, Go and Rust in one workflow), `java_hello.orbit`,
`csharp_hello.orbit`, and `http_ping.orbit` (the built-in HTTP adapter). Start
your own project with `orbit init my-project`.

## Features

**Language:** typed task contracts and schemas, imports, optional types with
`coalesce`, dependencies from data references, conditions (`when`), retries
with `non_retryable` codes, attempt, queue and workflow timeouts, idempotency
keys, durable `sleep`, compensation (Saga), fan-out (`for item in list`,
`max_parallel`), child workflows (`workflow "Name"`), typed signals and queries
for people and external systems in the loop (`signal`, `await`, `query`), and
deterministic `compute` steps with `map`/`filter`/`sum`. Errors come with
stable codes, source locations and suggestions.

**Checked before it runs:** beyond type-checking every step against its
worker's contract, the compiler finds missing compensations (`compensated_by`
on contracts), steps that can never run, signals nothing waits for and waits
without a timeout; `orbit check --strict` makes them build failures.
`orbit compat old.orbit new.orbit` reports breaking changes to inputs, outputs,
signals and queries, and the version bump they require.

**Runtime:** event-sourced and crash-safe. SQLite or PostgreSQL storage.
At-least-once delivery with reconciliation of uncertain attempts. The worker
protocol is served over HTTP/JSON and gRPC. Admin, shared-worker and per-worker
credentials, TLS and mutual TLS. OpenTelemetry traces, Prometheus metrics,
structured logs, and an opt-in HTTP adapter.

**Tooling:** the `orbit` CLI, a comment-preserving formatter (`orbit fmt`), a
language server (`orbit lsp`) with a VS Code extension, a Mermaid/Graphviz step
graph, and `orbit init` scaffolding.

**SDKs:** Python, TypeScript, Go, Rust, Java and C#. All six are tested
against the real runtime, including over mutual TLS.

## What's in the box

| Component | Path | |
|---|---|---|
| Language front end | `crates/orbit-syntax` | lexer, parser with error recovery, AST, diagnostics, formatter |
| Compiler | `crates/orbit-compiler` | imports, name resolution, type checker, dependency graph, child-workflow ordering, IR lowering |
| IR | `crates/orbit-ir` | `orbit-ir/1`: types, payload validation, pure expression evaluator, invariants, definition hash |
| Runtime | `crates/orbit-runtime` | engine and pure decider, SQLite/PostgreSQL, dispatch, fan-out, children, recovery, HTTP API, gRPC, identities, TLS/mTLS, OpenTelemetry |
| CLI | `crates/orbit-cli` (crate `orbit-lang`) | `init check validate compile fmt compat graph server deploy run status inspect logs signal query cancel ps workers token lsp` |
| SDKs | `sdks/python`, `sdks/typescript`, `sdks/go`, `sdks/java`, `sdks/csharp`, `crates/orbit-worker` | one per language |
| Worker protocol | `protocol/` | canonical `.proto`; HTTP/JSON and gRPC bindings |
| Specification | `specification/` | RFC-0001 with the implementation record, EBNF grammar |
| Docs and website | `docs/`, `website/` | the [documentation site](https://ajmalx0.github.io/Orbit/docs/) (mdBook sources in `docs/src`) and the landing page |
| VS Code extension | `editors/vscode` | language client for `orbit lsp` |

## Guarantees, precisely

* **Durable.** Every state change is an event committed to the database before
  it takes effect. State is rebuilt from history on restart.
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

Details: [Delivery guarantees](https://ajmalx0.github.io/Orbit/docs/runtime/guarantees.html).

## Developing

Open the repository in VS Code. `.vscode/` recommends extensions and provides
build, test and debug configurations, and `.devcontainer/` gives a container
with every toolchain preinstalled. One command runs every check CI runs:

```bash
./scripts/check.sh
```

That covers Rust format, lint and tests (including PostgreSQL when
`ORBIT_TEST_POSTGRES_URL` is set), each SDK's own tests, the formatter check,
and the end-to-end suite. The end-to-end tests start a real server and real
workers in six languages, `kill -9` the runtime mid-task, and exercise HTTPS,
mutual TLS and per-worker identities. Acceptance criteria are mapped to tests
in [RFC-0001 §15](specification/RFC-0001.md#15-testing-strategy-and-acceptance-criteria).
See [CONTRIBUTING.md](CONTRIBUTING.md).

## Status and license

Version 0.2: a single-node runtime. Next: multi-node execution sharing
PostgreSQL, and `orbit test` / `replay` / `simulate`
([RFC-0001 Appendix A.4](specification/RFC-0001.md#a4-known-gaps)).

Licensed under the [Apache License 2.0](LICENSE).
