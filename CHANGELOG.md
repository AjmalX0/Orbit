# Changelog

All notable changes are listed here. The project follows
[Semantic Versioning](https://semver.org/). The language, the IR (`orbit-ir/1`, `orbit-ir/2`)
and the worker protocol (`orbit.worker.v1`) are versioned separately, as
described in RFC-0001 §17.

## [0.2.0] — 2026-11-15

### Language
- **Signals and queries.** `signal <name> { ... }` declares a typed message a
  workflow accepts; an `await <name>` step waits for it durably (with
  `timeout`, failing with `SIGNAL_TIMEOUT`). Signals sent early are buffered and
  delivered in order. `query <name> = <expr>` declares a typed, read-only view
  of the execution state.
- **Compute steps.** `compute <expr>` evaluates a deterministic expression in
  the runtime, with no worker; its output type is inferred. New lambdas
  (`x => ...`) and built-ins `map`, `filter`, `find`, `any`, `all`, `sum`.
- **Static safety checks.** Task contracts can declare `compensated_by`; the
  compiler warns about missing or mismatched compensations (W0601, W0602),
  steps that never run or conditions that are always true (W0603, W0604),
  signals nobody awaits (W0605) and waits without a timeout (W0606).
  `orbit check --strict` / `orbit compile --strict` turn warnings into errors.
- New diagnostics E0315–E0318 and E0415.

### Tooling
- `orbit compat <old> <new>`: reports breaking changes between two versions
  of workflows and contracts (inputs, outputs, signals, queries) and the
  version bump they require; JSON output for CI.
- `orbit signal` and `orbit query`.
- `orbit fmt` keeps Windows (CRLF) line endings. The CLI exits quietly when its
  output pipe closes (`orbit logs --json | head`).

### Runtime
- `POST /v1/executions/{id}/signals/{name}` and
  `GET /v1/executions/{id}/queries/{name}`; history events `SignalReceived`,
  `SignalAwaitStarted`, `SignalDelivered`.
- `orbit-ir/2` for workflows that use the new features. Workflows that do not
  still compile to `orbit-ir/1` with unchanged definition hashes, so existing
  deployments are unaffected. The runtime accepts both.
- A failed compensation no longer stops the others: every remaining
  compensation runs, and each failure is reported.
- Graceful shutdown on `SIGTERM` as well as `SIGINT`; the container image sets
  `STOPSIGNAL SIGINT`.
- `float(Bool)` evaluates at run time, as the compiler allows.

### SDKs
- Python and TypeScript clients: `signal()` and `query()`.

### Project
- Documentation website with a landing page and complete documentation,
  deployed to GitHub Pages.
- `install.sh` explains how to install when a release has no binary for the
  platform (macOS: `cargo install orbit-lang`).

## [0.1.0] — 2026-11-15

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
