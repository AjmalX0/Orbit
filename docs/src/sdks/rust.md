# Rust

The Rust SDK, the `orbit-worker` crate, runs ORBIT task handlers written in
Rust. Handlers are typed: the SDK deserializes each task's input into your
`serde` type and serializes your output. Each poll loop runs on its own OS
thread, and the SDK handles registration, long polling, heartbeats,
cooperative cancellation, result delivery through runtime outages, and
graceful shutdown. Read [How workers work](overview.md) first for the
concepts this page relies on.

## Install

```bash
cargo add orbit-worker
cargo add serde --features derive
```

The crate requires Rust 1.89 or later. It depends on `serde`, `serde_json` and
`ureq`, and is blocking: it needs no async runtime.

## A minimal worker

This worker implements `text.fingerprint`, the Rust step of
[`examples/polyglot.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/polyglot.orbit):

```orbit
task "text.fingerprint" { version "1.0.0" input { text: String } output { hash: String, bytes: Int } }

step fingerprint { worker "text.fingerprint" using rust with { text: normalize.text } timeout 5s }
```

```rust
use orbit_worker::{TaskError, Worker};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Input {
    text: String,
}

#[derive(Serialize)]
struct Output {
    hash: String,
    bytes: usize,
}

/// 64-bit FNV-1a, a stable non-cryptographic fingerprint.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100000001b3)
    })
}

fn main() {
    let mut worker = Worker::builder("text-rust").build();
    worker.task("text.fingerprint", "1.0.0", |input: Input, ctx| {
        if input.text.is_empty() {
            return Err(TaskError::permanent("EMPTY_TEXT", "text must not be empty"));
        }
        ctx.log("fingerprinting");
        Ok(Output {
            hash: format!("{:016x}", fnv1a(input.text.as_bytes())),
            bytes: input.text.len(),
        })
    });
    worker.run().expect("worker failed");
}
```

Run it against a local runtime. The workflow has four steps in four
languages, so start the other three example workers from the repository root
as well:

```bash
orbit server &
cargo run &
PYTHONPATH=sdks/python python3 examples/workers/polyglot/text_worker.py &
node examples/workers/polyglot/text_worker.ts &
(cd examples/workers/polyglot/go && go run .) &

orbit run examples/polyglot.orbit \
  --input '{"text": "The quick brown fox jumps over the lazy dog. The dog sleeps."}'
```

The repository ships the same worker as an example:
`cargo run -p orbit-worker --example text_worker`.

## Registering tasks

```rust
pub fn task<I, O, F>(&mut self, name: &str, version: &str, handler: F) -> &mut Self
where
    I: DeserializeOwned,
    O: Serialize,
    F: Fn(I, &Context) -> Result<O, TaskError> + Send + Sync + 'static;
```

`task` registers `handler` for `name@version` and returns the worker, so calls
can be chained. The name and version must match the `task` declaration in
your `.orbit` files exactly. Registering the same name twice panics.

Handlers are `Fn`, and several poll threads can call one handler at the same
time, so shared mutable state needs a `Mutex` or an atomic.

## Handlers and results

The SDK deserializes the task input into `I` with `serde_json`. Use
`serde_json::Value` as `I` to receive the raw input. Input that does not
deserialize fails the attempt with `INVALID_INPUT`, which is not retried.

Return `Ok(output)` to complete the attempt:

- `O` is serialized with `serde_json` and must be a JSON object, so return a
  struct, a map, or a `serde_json::Value` object such as `json!({ ... })`.
- `Ok(())` and other values that serialize to `null` report `{}`.
- Output that is not an object, or fails to serialize, fails the attempt with
  `INVALID_RESULT`, which is not retried.
- The runtime validates the output against the contract's output schema; a
  mismatch fails the step with `OUTPUT_CONTRACT_VIOLATION`.

## Returning task errors

Return `Err(TaskError)` to fail an attempt with a structured error:

```rust
use serde_json::json;

if amount > 5000 {
    return Err(TaskError::permanent("CARD_DECLINED", "limit exceeded")
        .with_details(json!({ "limit": 5000 })));
}
```

| Constructor | Retryable |
|---|---|
| `TaskError::retryable(code: &str, message: impl Into<String>)` | yes |
| `TaskError::permanent(code: &str, message: impl Into<String>)` | no, fails the step whatever its retry policy |

`with_details(self, details: Value) -> Self` attaches a JSON value that is
stored with the error in the execution history. A panic in a handler is caught
and reported as a retryable `PANIC` whose message is the panic payload; the
default panic hook still prints it to standard error.

## Context

The second handler argument, `&Context`, describes the attempt.

| Method | Returns | Meaning |
|---|---|---|
| `execution_id()` | `&str` | The workflow execution. |
| `step_id()` | `&str` | The step, or `compensate:<step>` for a compensation; `step[i]` for a fan-out element. |
| `attempt()` | `u32` | 1-based attempt number. Redeliveries keep the number. |
| `idempotency_key()` | `&str` | Stable across retries and redeliveries. Use it for side effects. |
| `redelivery()` | `bool` | `true` when the outcome of an earlier delivery of this attempt is unknown. |
| `is_cancelled()` | `bool` | `true` once the runtime asked the attempt to stop. |
| `remaining_ms()` | `Option<u64>` | Milliseconds left before the attempt's deadline. |
| `log(message: &str)` | `()` | Writes one JSON line to standard error with the execution, step and attempt. |

The public field `assignment: Assignment` holds the full task message:

| Field | Type | Meaning |
|---|---|---|
| `token` | `String` | Identifies this delivery. |
| `execution_id` | `String` | The workflow execution. |
| `workflow` | `String` | Workflow name. |
| `workflow_version` | `String` | Workflow version. |
| `step_id` | `String` | The step. |
| `task_name` | `String` | The task name. |
| `contract_version` | `String` | The task's contract version. |
| `input` | `serde_json::Value` | The raw input. |
| `idempotency_key` | `String` | The idempotency key. |
| `attempt` | `u32` | The attempt number. |
| `redelivery` | `bool` | The redelivery flag. |
| `deadline_ms` | `Option<u64>` | Absolute deadline in Unix epoch milliseconds. |

## Cancellation and timeouts

When an attempt times out, or its workflow is cancelled or fails, the runtime
lists the attempt in the next heartbeat response and `ctx.is_cancelled()`
starts returning `true`. The SDK does not interrupt the handler and does not
act on the deadline by itself, so check the flag between units of work and
bound blocking calls with `ctx.remaining_ms()`:

```rust
worker.task("reports.render", "1.0.0", |input: Report, ctx| {
    let mut pages = 0;
    for section in &input.sections {
        if ctx.is_cancelled() {
            return Err(TaskError::retryable("CANCELLED", "stopped by the runtime"));
        }
        render(section, ctx.remaining_ms())?;
        pages += 1;
    }
    Ok(serde_json::json!({ "pages": pages }))
});
```

Heartbeats run on their own thread, so a long handler keeps its session alive.

## Concurrency

`concurrency(n)` (default 4) sets the number of poll threads, and therefore
the maximum number of tasks this worker runs at once. Values below 1 become 1.
Handlers run on those threads.

## Credentials and TLS

`Worker::builder` reads the environment; builder methods override it.

| Builder method | Environment | Default | Meaning |
|---|---|---|---|
| `server(url: &str)` | `ORBIT_SERVER` | `http://127.0.0.1:7233` | Runtime URL. |
| `token(token: &str)` | `ORBIT_WORKER_TOKEN`, then `ORBIT_TOKEN` | none | Bearer token. |
| `ca_pem(pem: Vec<u8>)` | `ORBIT_CA_CERT` (a path) | system roots | PEM CA certificates. When set, they are the only roots trusted. |
| `client_identity(cert_pem: Vec<u8>, key_pem: Vec<u8>)` | `ORBIT_CLIENT_CERT` and `ORBIT_CLIENT_KEY` (paths) | none | Client certificate chain and private key for mutual TLS. |

```rust
let worker = Worker::builder("payments-1")
    .server("https://orbit.internal:7233")
    .ca_pem(std::fs::read("/etc/orbit/ca.pem")?)
    .client_identity(
        std::fs::read("/etc/orbit/payments-1.pem")?,
        std::fs::read("/etc/orbit/payments-1.key")?,
    )
    .build();
```

The environment variables for TLS are read as file paths. A file that cannot
be read is ignored without an error, and the client certificate is used only
when both `ORBIT_CLIENT_CERT` and `ORBIT_CLIENT_KEY` are set. Read the files
yourself, as above, to surface errors. With a per-worker identity from a
workers file, pass the identity's `id` to `Worker::builder`.

## Logging

The SDK writes registration, runtime warnings, poll and delivery failures, and
ignored results to standard error with the prefix `[orbit]`. It has no logging
configuration and does not use the `log` or `tracing` crates.

## Shutdown

`run()` blocks the calling thread. `stop()` asks it to return: polling stops,
running handlers finish and deliver their results, the worker unregisters, and
`run()` returns `Ok(())`. A poll that is already waiting is not interrupted,
so `run()` can take up to the poll wait (20 seconds by default) to return.

`Worker` is cheap to clone and clones share state, so stop it from another
thread or a signal handler. The SDK installs no signal handlers itself. With
the `ctrlc` crate:

```rust
let stopper = worker.clone();
ctrlc::set_handler(move || stopper.stop()).expect("cannot install signal handler");
worker.run()?;
```

The `ctrlc` crate's `termination` feature also handles `SIGTERM`. A hard kill
is safe as well: delivery is at least once, so unfinished tasks are
redelivered.

## API reference

### `Worker`

| Item | Meaning |
|---|---|
| `Worker::builder(worker_id: &str) -> WorkerBuilder` | Starts a builder with defaults from the environment. |
| `task(&mut self, name, version, handler) -> &mut Self` | Registers a handler. Panics on a duplicate name. |
| `run(&self) -> Result<(), ApiError>` | Registers, polls and blocks until `stop()`. Returns `Err` with code `NO_TASKS` when no task is registered, and `Err` when the runtime refuses registration. Connection errors and 5xx responses are retried. |
| `stop(&self)` | Asks `run()` to return after in-flight tasks finish. |

`Worker` implements `Clone`.

### `WorkerBuilder`

| Method | Default | Meaning |
|---|---|---|
| `server(url: &str)` | `ORBIT_SERVER`, then `http://127.0.0.1:7233` | Runtime URL. |
| `token(token: &str)` | `ORBIT_WORKER_TOKEN`, then `ORBIT_TOKEN` | Bearer token. |
| `ca_pem(pem: Vec<u8>)` | `ORBIT_CA_CERT` | Trusted CA certificates. |
| `client_identity(cert_pem: Vec<u8>, key_pem: Vec<u8>)` | `ORBIT_CLIENT_CERT`, `ORBIT_CLIENT_KEY` | Mutual TLS identity. |
| `concurrency(n: usize)` | `4` | Number of poll threads. |
| `runtime(runtime: &str)` | `"rust"` | Announced runtime, matched against `using <language>`. |
| `poll_wait(d: Duration)` | 20 seconds | `wait_ms` of each long poll. The runtime caps it at 60 seconds. |
| `build(self) -> Worker` | | Builds the worker. |

### `TaskError`

```rust
pub struct TaskError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details: Option<Box<serde_json::Value>>,
}
```

Implements `Display` (`"<code>: <message>"`), `std::error::Error`, `Clone`
and `PartialEq`. See [Returning task errors](#returning-task-errors).

### `ApiError`

```rust
pub struct ApiError {
    pub status: u16,     // HTTP status; 0 for a connection failure
    pub code: String,    // the runtime's error code, CONNECTION, or HTTP_ERROR
    pub message: String,
}
```

### Other items

| Item | Meaning |
|---|---|
| `Context` | See [Context](#context). |
| `Assignment` | The task message, `Context::assignment`. |
| `VERSION` | The crate version. |

## Testing

Write handlers as functions so that you can call the business logic without a
`Context`, which the SDK alone constructs:

```rust
fn fingerprint(text: &str) -> Output {
    Output { hash: format!("{:016x}", fnv1a(text.as_bytes())), bytes: text.len() }
}

#[test]
fn fingerprints_are_stable() {
    assert_eq!(fingerprint("abc").hash, fingerprint("abc").hash);
}
```

For an integration test, start a runtime, register your workflow, run the
worker on a thread (`let runner = worker.clone(); std::thread::spawn(move ||
runner.run())`), start executions through the HTTP API, and call
`worker.stop()` when done. A short `poll_wait` makes `stop()` take effect
quickly. The crate's own test, `crates/orbit-worker/tests/runtime.rs`, starts
an in-process runtime with an in-memory store and follows this pattern.

## See also

- [How workers work](overview.md)
- [Worker protocol](../reference/protocol.md)
- [Task contracts](../language/tasks.md)
- [Retries and timeouts](../language/retries-timeouts.md)
- [Idempotency](../language/idempotency.md)
- [Security](../runtime/security.md)
- [Polyglot workflows](../guides/polyglot.md)
