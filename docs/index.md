---
title: ORBIT
---

## One workflow. Every language. Reliable execution.

ORBIT is a declarative programming language and durable runtime for orchestrating work across services written in different languages. You describe what should happen in a `.orbit` file. The compiler type-checks every step against its worker's contract, and the runtime executes it durably: retries, timeouts, compensation, crash recovery and a complete audit history. Your business logic stays in Python, TypeScript, Go, Rust, Java, C# or anything that can speak HTTP.

**[Download v0.1.0](https://github.com/AjmalX0/Orbit/releases/latest)** · [Quick start](https://github.com/AjmalX0/Orbit#quick-start) · [Source on GitHub](https://github.com/AjmalX0/Orbit)

## What it looks like

```
step analyze_risk {            // runs in parallel with validate_order
  worker "risk.analyze"
  using python
  with { customer_id: input.customer_id, amount: input.amount }
  retry { attempts: 3, backoff: exponential, initial_delay: 1s }
}

step process_payment {
  worker "payments.charge"
  using python
  after validate_order
  when analyze_risk.score < 0.8
  with { order_id: input.order_id, amount: input.amount, currency: "USD" }
  idempotency_key: "charge-" + input.order_id
  compensate {
    worker "payments.refund"
    with { transaction_id: process_payment.transaction_id, amount: input.amount }
  }
}
```

Mistakes are caught before anything runs:

```
error[E0405]: value may be `null` but `String` is required
  = help: provide a fallback with `coalesce(value, default)`, or declare the target as optional with `?`
```

## Install

| Package | |
|---|---|
| CLI / runtime | `cargo install orbit-lang`, the install script, [GitHub Releases](https://github.com/AjmalX0/Orbit/releases/latest), `ghcr.io/ajmalx0/orbit` |
| Python | `pip install orbit-lang-sdk` |
| TypeScript / JavaScript | `npm install orbit-lang` |
| Go | `go get github.com/AjmalX0/Orbit/sdks/go` |
| Rust | `cargo add orbit-worker` |
| Java | `io.github.ajmalx0:orbit-sdk` (Maven Central) |
| C# / .NET | `dotnet add package OrbitLang.Sdk` |
| VS Code | `orbit-lang.vsix` from GitHub Releases |

```
curl -fsSL https://github.com/AjmalX0/Orbit/releases/latest/download/install.sh | sh
docker run -p 7233:7233 -e ORBIT_TOKEN=change-me -v orbit-data:/data ghcr.io/ajmalx0/orbit
```

## Documentation

- [Language reference](language.md)
- [Runtime semantics](runtime.md)
- [Diagnostics](diagnostics.md)
- [Specification (RFC-0001)](https://github.com/AjmalX0/Orbit/tree/main/specification)
- [Examples](https://github.com/AjmalX0/Orbit/tree/main/examples)

## Guarantees

- **Durable.** Every state change is an event committed to the database before it takes effect; state is rebuilt from history on restart.
- **At-least-once delivery, never "exactly-once".** Every task carries an idempotency key that is stable across retries and redeliveries.
- **Immutable definitions.** `name@version` is bound to a content hash; running executions keep the definition they started with.
- **Contract safety at both ends.** The compiler checks every step's input and reference; the runtime validates inputs and worker outputs against the same types.

Version 0.1 · single-node runtime · [Apache License 2.0](https://github.com/AjmalX0/Orbit/blob/main/LICENSE)
