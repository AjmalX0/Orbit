# Changelog

All notable changes are listed here. The project follows
[Semantic Versioning](https://semver.org/). The language, the IR (`orbit-ir/1`)
and the worker protocol (`orbit.worker.v1`) are versioned separately, as
described in RFC-0001 §17.

## [0.1.0] — unreleased

First public version.

### Language and compiler
- Workflows, steps, task contracts, schemas, imports, defaults.
- Static structural type checking with optional types, `coalesce`, and
  null-propagating member access.
- Dependencies from `after` and from data references; cycle detection.
- Retries (fixed or exponential, `non_retryable`), per-attempt and queue timeouts,
  workflow timeouts, idempotency keys, conditions (`when`), durable `sleep`,
  compensation (Saga).
- `for item in <list>` fan-out with `max_parallel`, and child workflows
  (`workflow "Name"`).
- `orbit-ir/1`: deterministic, hashed and validated.
- Located diagnostics with stable codes and suggestions.

### Runtime
- Event-sourced durable engine with a pure decider; crash recovery and
  reconciliation of uncertain attempts.
- SQLite and PostgreSQL storage (TLS via `sslmode`).
- HTTP API, plus the worker protocol over HTTP/JSON and gRPC.
- Admin, shared-worker and per-worker credentials; TLS and mutual TLS.
- OpenTelemetry trace export (OTLP/HTTP), Prometheus metrics, structured logs.
- Built-in HTTP adapter (opt-in).

### Tooling
- `orbit` CLI: `init check validate compile fmt graph server deploy run status
  inspect logs cancel ps workers token lsp`.
- Language server and VS Code extension.
- Worker SDKs: Python, TypeScript, Go, Rust, Java and C#.
