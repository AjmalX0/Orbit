# How workers work

A worker is a process you run that implements one or more task contracts. It
connects to the ORBIT runtime, announces the tasks it serves, long-polls for
work, runs your handler, and reports the result. Workers never own workflow
state: the runtime decides what runs next, records every outcome in the
execution history, and redelivers work when a worker disappears. This page
describes the model that all six official SDKs share. The language pages
document each SDK's API, and the [worker protocol](../reference/protocol.md)
reference describes the wire format for writing a worker in another language.

## The worker lifecycle

Every SDK runs the same loop over the HTTP/JSON binding of the worker protocol:

```text
worker                                         runtime
  │ POST /v1/workers/register  ───────────────▶ │  worker_id, runtime, contracts[]
  │ ◀─────────────── worker_session_id, heartbeat_interval_ms, session_timeout_ms
  │
  │ POST /v1/workers/poll  (long poll) ───────▶ │  worker_session_id, wait_ms
  │ ◀──────────────── 200 task assignment   or   204 nothing within wait_ms
  │   run the handler
  │ POST /v1/tasks/complete  or  /v1/tasks/fail ▶ │  token, output | error
  │ ◀──────────────────────────────── accepted, reason
  │
  │ POST /v1/workers/heartbeat  (every heartbeat_interval_ms)
  │ ◀──────────────────────────────── accepted, cancel[]
  │ POST /v1/workers/unregister  (on shutdown)
```

1. **Register.** The worker sends its `worker_id`, the runtime it represents
   (for example `python`), and the list of task contracts it implements. The
   runtime returns a `worker_session_id` and the heartbeat interval. With the
   default `--session-timeout 30`, the session timeout is 30 seconds and the
   heartbeat interval is 5 seconds (one sixth of the timeout, at least 500 ms).
2. **Poll.** Each poll loop asks for a task and waits up to `wait_ms` (the SDKs
   use 20 seconds; the runtime caps it at 60 seconds). The runtime answers as
   soon as a matching task is queued, or with `204 No Content` when the wait
   ends. Workers pull work, so they need no inbound ports and work behind NAT.
3. **Execute.** The SDK decodes the task input, calls your handler with the
   input and a context object, and waits for it to return.
4. **Complete or fail.** A returned value is sent to `/v1/tasks/complete`. A
   task error or an unexpected exception is sent to `/v1/tasks/fail`.
5. **Heartbeat.** A background loop calls `/v1/workers/heartbeat` at the
   interval the runtime asked for. Polls and heartbeats both count as contact.
   The heartbeat response carries the tokens of attempts the worker should
   stop (see [Timeouts, heartbeats and cancellation](#timeouts-heartbeats-and-cancellation)).
6. **Unregister.** On shutdown the SDK stops polling, lets in-flight tasks
   finish, and unregisters the session.

If the runtime restarts, it forgets worker sessions. The next session call
returns `404 UNKNOWN_SESSION`; every SDK then registers again and continues.
Task tokens stay valid across sessions, so a result produced during the
restart is still delivered.

## Task names and contract versions

A worker announces each task as a pair of `task_name` and `contract_version`,
for example `payments.charge` at `1.0.0`. Both must match the `task`
declaration in your `.orbit` files exactly:

```orbit
task "payments.charge" {
    version "1.0.0"
    input  { order_id: String, amount: Float }
    output { transaction_id: String }
}
```

A worker receives a task only when it announced the same name and the same
contract version that the running workflow uses. When a worker announces a
version that no registered workflow uses, registration still succeeds, and the
response includes a warning that every SDK logs:

```text
runtime: `greeting.compose` is announced at version 2.0.0 but registered workflows use 1.0.0; this worker will not receive those tasks
```

Names that start with `orbit.` are built-in tasks, and contract names that
refer to child workflows are reserved. Announcing either fails registration
with `400 RESERVED_TASK`. A worker must announce at least one task.

The runtime validates the output of every completed task against the
contract's output schema, after applying schema defaults. Output that does not
match fails the attempt with `OUTPUT_CONTRACT_VIOLATION`, which is never
retried. See [Task contracts](../language/tasks.md) and
[Versioning](../language/versioning.md).

## Routing with `using`

A step can name the language of the worker that should run it:

```orbit
step normalize { worker "text.normalize" using python with { text: input.text } }
```

When a step has `using <language>`, the runtime only hands the task to workers
whose announced runtime equals that language, compared case-insensitively.
`using any` and a step without `using` accept a worker of any runtime. Each
SDK announces its own language by default:

| SDK | Default runtime |
|---|---|
| Python | `python` |
| TypeScript and JavaScript | `typescript` |
| Go | `go` |
| Rust | `rust` |
| Java | `java` |
| C# | `csharp` |

Every SDK lets you override the announced runtime. For example, a Node.js
worker that serves steps declared `using javascript` must set its runtime to
`javascript`, because the TypeScript SDK announces `typescript`.

When several workers can serve a task, the runtime hands the oldest queued
matching task to whichever worker polls first.

## Concurrency

Each SDK runs a fixed number of poll loops, set by its concurrency option
(default 4 in every SDK). A loop polls, runs one task to completion, reports
the result, and polls again, so the concurrency setting is the maximum number
of tasks the worker runs at the same time. A busy worker stops polling, which
gives natural backpressure: queued work waits for, or goes to, another worker.

How the loops run depends on the language:

| SDK | Poll loops run on |
|---|---|
| Python | one thread per loop; `async def` handlers run in a fresh event loop per task |
| TypeScript | asynchronous loops on the Node.js event loop; CPU-bound handlers block every loop |
| Go | one goroutine per loop |
| Rust | one OS thread per loop |
| Java | one platform thread per loop |
| C# | one thread-pool task per loop |

To scale beyond one process, run more worker processes. Each registers its own
session, and the runtime distributes tasks among them.

## Delivery guarantees and idempotency

ORBIT delivers tasks **at least once**. A handler can run more than once for
the same step:

- **Retries.** A failed attempt that the retry policy allows is scheduled
  again with a higher `attempt` number.
- **Redeliveries.** When the runtime cannot know the outcome of an attempt
  (the worker session went silent for longer than the session timeout, the
  worker unregistered while the task was running, or the runtime restarted and
  nobody reported the result), it delivers the *same* attempt again, with the
  same attempt number and `redelivery: true`. A redelivery does not use up a
  retry.

Every task carries an `idempotency_key`. By default it is
`<execution_id>/<step>`; a step can set its own with `idempotency_key:`. The
key is identical across retries and redeliveries of a step, so use it to make
external side effects idempotent: pass it to a payment provider's idempotency
header, use it as a unique key in your database, or remember results by it.

```python
@worker.task("payments.charge", version="1.0.0")
def charge(input, ctx):
    existing = db.find_charge(ctx.idempotency_key)
    if existing:
        return existing                      # a retry or redelivery
    return db.create_charge(ctx.idempotency_key, input["amount"])
```

The runtime reconciles results. It accepts a result for any token issued for a
step that is not yet resolved, because the work really happened: a late
success from an attempt that already timed out still completes the step.
Failures count only for the attempt currently in flight. A result that arrives
after the step is resolved is recorded as `StaleResultIgnored` and answered
with `accepted: false`; the SDKs log it and move on. See
[Idempotency](../language/idempotency.md) and
[Delivery guarantees](../runtime/guarantees.md).

## Errors

A handler fails an attempt by raising or returning a task error with three
parts:

| Field | Meaning |
|---|---|
| `code` | A stable, machine-readable string, for example `CARD_DECLINED`. |
| `message` | A human-readable description. |
| `retryable` | Whether the runtime may retry the attempt. Defaults to `true` in every SDK. |
| `details` | Optional JSON value stored with the error in the execution history. |

### Retryable and non-retryable errors

The runtime retries a failed attempt only when all of these hold:

1. the error is retryable,
2. its code is not listed in the step's `retry { non_retryable: [...] }`,
3. attempts remain (`attempts` counts the first attempt; the default is 1,
   which means no retries), and
4. the workflow is not being aborted.

`non_retryable` matches error codes exactly and case-sensitively, so a code
listed there stops retries even when the worker marked the error retryable:

```orbit
step charge {
    worker "payments.charge"
    retry {
        attempts: 5
        backoff: exponential
        initial_delay: 500ms
        non_retryable: ["CARD_DECLINED"]
    }
}
```

Mark an error non-retryable in the worker when retrying can never help, such
as invalid input or a declined card. Leave it retryable for transient
problems, such as a timeout calling another service, and let the workflow's
retry policy decide. See [Retries and timeouts](../language/retries-timeouts.md).

### Error codes the SDKs produce

The SDKs report these codes on your behalf:

| Code | Retryable | Produced when |
|---|---|---|
| `UNHANDLED_EXCEPTION` | yes | The handler raised an exception or returned an error that is not a task error. The details include a stack trace where the language provides one. |
| `PANIC` | yes | A Go or Rust handler panicked. |
| `INVALID_INPUT` | no | A typed handler in Rust, Java or C# could not decode the input into its input type. In Go, you return this code yourself when `Decode` fails. |
| `INVALID_RESULT` | no | The handler returned something that is not a JSON object. |
| `UNKNOWN_TASK` | yes | The runtime delivered a task the worker did not register. |
| `CANCELLED` | yes in Java and C#, no in TypeScript | The handler stopped because the attempt was cancelled, and let the SDK's cancellation error propagate. |

The runtime itself produces these codes, which you can list in
`non_retryable` or see in `orbit status`:

| Code | Retried | Meaning |
|---|---|---|
| `TIMEOUT` | yes | The attempt exceeded the step's `timeout`. |
| `OUTPUT_CONTRACT_VIOLATION` | never | The worker's output does not match the contract. |
| `INPUT_CONTRACT_VIOLATION` | never | The step's input does not match the contract. |
| `EXPRESSION_ERROR` | never | An expression failed to evaluate. |
| `QUEUE_TIMEOUT` | never | No worker picked the task up within `queue_timeout`. |
| `ADAPTER_DISABLED` | never | An `orbit.http.request` step ran on a runtime without `--enable-http-adapter`. |

## Timeouts, heartbeats and cancellation

A step's `timeout` limits one attempt, from dispatch to result. The runtime
sends it to the worker as `timeout_ms`, together with `deadline_ms`, the
absolute deadline in Unix epoch milliseconds. When the deadline passes, the
runtime fails the attempt with `TIMEOUT` (retryable) and schedules a retry if
the policy allows one.

Cancellation is cooperative. When an attempt times out, or its workflow is
cancelled or fails, the runtime puts the attempt's token in the `cancel` list
of the next heartbeat response. The SDK then signals your handler:

| SDK | Cancellation signal | Fires on deadline locally |
|---|---|---|
| Python | `ctx.cancelled` (`threading.Event`), `ctx.is_cancelled` | no |
| TypeScript | `ctx.signal` (`AbortSignal`), `ctx.cancelled` | no |
| Go | the handler's `context.Context` | yes |
| Rust | `ctx.is_cancelled()` | no |
| Java | `ctx.isCancelled()`, `ctx.cancellation()`, thread interrupt | yes |
| C# | `ctx.CancellationToken` | yes |

The runtime cannot stop an external operation for you. Check the signal
between units of work, pass it to I/O calls that accept one, and use
`remaining_ms` (or the language's equivalent) to bound calls to other
services. A heartbeat cancellation only reaches the handler at the next
heartbeat, up to one heartbeat interval later.

A worker whose session is silent for longer than the session timeout is
considered lost, and its in-flight attempts are redelivered elsewhere. The
SDKs heartbeat on a separate thread or task, so a long-running handler keeps
its session alive without doing anything.

## Credentials and TLS

The runtime supports three ways for a worker to authenticate. Each SDK sends
credentials the same way on every request.

**Shared worker token.** Start the runtime with `--token` (the admin token)
and `--worker-token`, and give worker hosts the worker token. A worker token
can only call `/v1/workers/*` and `/v1/tasks/*`; any other endpoint returns
`403 FORBIDDEN`, so a compromised worker host cannot start, cancel or read
executions.

**Per-worker identities.** A workers file (`--workers-file workers.toml`)
gives each worker its own credential and limits. `orbit token --worker
payments-1 --task 'payments.*'` prints a random token and the matching entry:

```toml
[[worker]]
id = "payments-1"
token_sha256 = "3cfba7de6b6d9594a537cd712545cf82b225be7d53660269f397568fc0e5a3d1"
tasks = ["payments.*"]       # exact names, `prefix.*` or `*`
runtime = "python"           # optional
```

A worker authenticated this way can register only under its own `id`, announce
only tasks its patterns allow, use only its own sessions, and report results
only for tokens issued to it. Anything else returns `403 FORBIDDEN`. Set the
SDK's worker id to the identity's `id`.

**Mutual TLS.** When the runtime serves HTTPS with `--tls-client-ca`, a worker
can present a client certificate signed by that CA instead of a token. The
runtime matches the certificate's DNS SANs and subject common name against
`client_cert` entries in the workers file.

To trust a runtime that uses a certificate from a private CA, give the SDK the
CA certificate in PEM form. The CA file must hold a real CA certificate
(`basicConstraints=CA:TRUE`, `keyCertSign`), not the server certificate.

See [Security](../runtime/security.md) for how to configure the runtime side.

## Environment variables

Every SDK reads its connection settings from the environment when you do not
set them in code. An option set in code always wins. Empty variables count as
unset.

| Variable | Purpose |
|---|---|
| `ORBIT_SERVER` | Runtime URL. Default `http://127.0.0.1:7233`. A trailing `/` is removed. |
| `ORBIT_WORKER_TOKEN` | Bearer token: the shared worker token or a per-worker token. Takes precedence over `ORBIT_TOKEN`. |
| `ORBIT_TOKEN` | Bearer token used when `ORBIT_WORKER_TOKEN` is unset. |
| `ORBIT_CA_CERT` | Path of a PEM CA certificate to trust for HTTPS. |
| `ORBIT_CLIENT_CERT` | Path of a PEM client certificate for mutual TLS. |
| `ORBIT_CLIENT_KEY` | Path of the PEM private key for `ORBIT_CLIENT_CERT`. |

The TLS variables differ slightly between SDKs:

| SDK | `ORBIT_CA_CERT` | `ORBIT_CLIENT_KEY` |
|---|---|---|
| Python | Trusted instead of the system store. | Optional; without it the key is read from the certificate file. |
| TypeScript | Used only together with a client certificate, and then replaces Node.js's default CAs. Without a client certificate, set `NODE_EXTRA_CA_CERTS`. | Optional; defaults to the certificate file. |
| Go | Added to the system pool. | Required with a certificate. |
| Rust | Trusted instead of the system store. | Required; the certificate is ignored without it. |
| Java | Trusted in addition to the system store. | Required with a certificate; must be unencrypted PKCS#8. |
| C# | Trusted in addition to the system store. | Optional; without it the key is read from the certificate file. |

The Python SDK also reads `ORBIT_LOG_LEVEL` (default `INFO`) when
`Worker.run()` configures logging.

## Graceful shutdown

Every SDK shuts down in the same order: it stops starting new polls, lets the
tasks already running finish and delivers their results, then unregisters the
session. Unregistering makes the runtime redeliver any task the worker still
held, immediately instead of after the session timeout.

How you trigger shutdown, and what happens to running handlers, differs:

| SDK | Trigger | Running handlers | Open long poll |
|---|---|---|---|
| Python | SIGINT, SIGTERM, or `worker.stop()` | run to completion | waits for it to return (up to `poll_wait_ms`) |
| TypeScript | SIGINT, SIGTERM, or `worker.stop()` | run to completion | aborted |
| Go | cancel the context passed to `Run` | their context is cancelled | aborted |
| Rust | `worker.stop()` from another thread | run to completion | waits for it to return |
| Java | `worker.stop()`, typically from a shutdown hook | run to completion | interrupted |
| C# | cancel the token passed to `RunAsync` | their `CancellationToken` is cancelled | aborted |

Shutting down is always safe, even with a hard kill. Delivery is at least
once, so a task whose result never arrived is redelivered after the session
timeout.

## Result delivery through outages

If the runtime is unreachable or answers with a 5xx status when a worker
reports a result, the SDK retries with exponential backoff (200 ms doubling to
5 s). After a runtime restart the result is still accepted, because the
runtime reconciles results for unresolved steps. A 4xx response or
`accepted: false` is final and is not retried.

| SDK | Retries a result for |
|---|---|
| Python, TypeScript, Rust | as long as the worker runs; stops once shutdown begins |
| Go, C# | up to 10 minutes, including during shutdown |
| Java | up to `resultRetryTimeout` (default 10 minutes), including during shutdown |

## Comparing the SDKs

| | Python | TypeScript | Go | Rust | Java | C# |
|---|---|---|---|---|---|---|
| Package | `orbit-lang-sdk` (PyPI) | `orbit-lang` (npm) | `github.com/AjmalX0/Orbit/sdks/go` | `orbit-worker` (crates.io) | `io.github.ajmalx0:orbit-sdk` | `OrbitLang.Sdk` (NuGet) |
| Requires | Python 3.9 | Node.js 18 | Go 1.21 | Rust 1.89 | Java 17 | .NET 8 |
| Dependencies | none | none | none | `serde`, `serde_json`, `ureq` | Jackson | none |
| Handler input | `dict` | typed (unchecked) | `*orbit.Task`, `Decode` | serde-typed | Jackson-typed or `JsonNode` | `System.Text.Json`-typed or `JsonElement` |
| Async handlers | `async def` or plain | `async` or plain | blocking | blocking | blocking | `async` |
| Default worker id | `python-<host>-<pid>` | `typescript-<pid>` | `go-<host>-<pid>` | required | `java-<host>-<pid>` | `csharp-<host>-<pid>` |
| Context exposes `contract_version`, `timeout_ms` | no | no | yes | `contract_version` only | yes | yes |
| Handles signals itself | yes | yes | no | no | no | no |
| Client for starting workflows | `Client` | `Client` | no | no | no | no |
| Heartbeats while draining | no | no | no | no | yes | no |

During shutdown, only the Java SDK keeps heartbeating until the last task
finishes. In the other SDKs, a task that keeps running for longer than the
session timeout after shutdown begins can be redelivered to another worker,
and the first worker's late result is then reconciled like any other.

## See also

- [Python](python.md), [TypeScript and JavaScript](typescript.md), [Go](go.md),
  [Rust](rust.md), [Java](java.md), [C# and .NET](csharp.md)
- [Worker protocol](../reference/protocol.md)
- [Task contracts](../language/tasks.md)
- [Retries and timeouts](../language/retries-timeouts.md)
- [Idempotency](../language/idempotency.md)
- [Delivery guarantees](../runtime/guarantees.md)
- [Security](../runtime/security.md)
- [Polyglot workflows](../guides/polyglot.md)
