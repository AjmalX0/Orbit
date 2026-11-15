# TypeScript and JavaScript

The TypeScript SDK, `orbit-lang`, runs ORBIT task handlers on Node.js. It has
no dependencies, is built on `fetch` and `AbortController`, ships type
declarations, and works from both TypeScript and JavaScript. It handles
registration, long polling, heartbeats, cancellation through an
`AbortSignal`, result delivery through runtime outages, and graceful shutdown
on `SIGINT` and `SIGTERM`. It also includes a small `Client` for starting
workflows. Read [How workers work](overview.md) first for the concepts this
page relies on.

## Install

```bash
npm install orbit-lang
```

The package requires Node.js 18 or later and is an ES module: load it with
`import`, not `require`. Node.js 22.18 and later run `.ts` files directly, so
you can run a TypeScript worker with `node worker.ts` without a build step.

## A minimal worker

This worker implements `text.tokenize`, the TypeScript step of
[`examples/polyglot.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/polyglot.orbit):

```orbit
task "text.tokenize" { version "1.0.0" input { text: String } output { tokens: List<String> } }

step tokenize { worker "text.tokenize" using typescript with { text: normalize.text } }
```

```ts
import { TaskError, Worker } from "orbit-lang";

const worker = new Worker({ workerId: "text-typescript" });

worker.task<{ text: string }, { tokens: string[] }>(
  "text.tokenize",
  { version: "1.0.0" },
  async (input, ctx) => {
    if (input.text.length === 0) {
      throw new TaskError("EMPTY_TEXT", "text must not be empty", { retryable: false });
    }
    ctx.log("tokenizing", { chars: input.text.length });
    return { tokens: input.text.split(" ").filter((t) => t.length > 0) };
  },
);

await worker.run();
```

Run it against a local runtime. The workflow has four steps in four
languages, so start the other three example workers from the repository root
as well:

```bash
orbit server &
node text_worker.ts &
PYTHONPATH=sdks/python python3 examples/workers/polyglot/text_worker.py &
(cd examples/workers/polyglot/go && go run .) &
cargo run -p orbit-worker --example text_worker &

orbit run examples/polyglot.orbit \
  --input '{"text": "The quick brown fox jumps over the lazy dog. The dog sleeps."}'
```

## Registering tasks

```ts
worker.task<Input, Output>(name, { version }, handler);
```

`task` registers `handler` for `name@version` and returns the worker, so calls
can be chained. The second argument is required; when `version` is omitted it
defaults to `"1.0.0"`. The name and version must match the `task` declaration
in your `.orbit` files exactly. Registering the same name twice throws.
Register every task before calling `run()`.

The type parameters describe the input and output for the compiler only. The
SDK does not validate the input at run time; the runtime has already validated
it against the contract.

## Handlers and results

A handler receives the task input and a [`Context`](#context), and returns the
output object, or a promise of it:

```ts
worker.task("inventory.reserve", { version: "1.0.0" }, async (input, ctx) => {
  return { reservation_id: `res-${input.order_id}` };
});
```

- When the runtime sends no input, `input` is `{}`.
- Returning `undefined` or `null` reports `{}`.
- Returning a non-object or an array fails the attempt with `INVALID_RESULT`,
  which is not retried.
- The runtime validates the output against the contract's output schema; a
  mismatch fails the step with `OUTPUT_CONTRACT_VIOLATION`.

## Raising task errors

Throw `TaskError` to fail an attempt with a structured error:

```ts
import { TaskError } from "orbit-lang";

throw new TaskError("CARD_DECLINED", "limit exceeded", {
  retryable: false,
  details: { limit: 5000 },
});
```

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `code` | `string` | required | Error code, matched exactly by the step's `non_retryable` list. |
| `message` | `string` | `""` | Human-readable message. |
| `options.retryable` | `boolean` | `true` | `false` fails the step immediately, whatever its retry policy. |
| `options.details` | `Json` | `undefined` | Stored with the error in the execution history. |

Any other thrown value is reported as a retryable `UNHANDLED_EXCEPTION`. The
message is `<name>: <message>` for an `Error`, and `details.stack` holds the
first lines of the stack trace. The SDK also logs the error through
`logger.error`.

## Context

| Member | Type | Meaning |
|---|---|---|
| `token` | `string` | Identifies this delivery. The SDK echoes it in the result. |
| `executionId` | `string` | The workflow execution. |
| `workflow` | `string` | Workflow name. |
| `workflowVersion` | `string` | Workflow version. |
| `stepId` | `string` | The step, or `compensate:<step>` for a compensation; `step[i]` for a fan-out element. |
| `taskName` | `string` | The task name. |
| `attempt` | `number` | 1-based attempt number. Redeliveries keep the number. |
| `idempotencyKey` | `string` | Stable across retries and redeliveries. Use it for side effects. |
| `redelivery` | `boolean` | `true` when the outcome of an earlier delivery of this attempt is unknown. |
| `deadlineMs` | `number \| undefined` | Absolute deadline in Unix epoch milliseconds, when the step has a `timeout`. |
| `signal` | `AbortSignal` | Aborted when the runtime asks the attempt to stop. |
| `cancelled` | `boolean` (getter) | `true` once `signal` is aborted. |
| `remainingMs()` | `number \| undefined` | Milliseconds left before `deadlineMs`, never negative. |
| `log(message, fields?)` | `void` | Writes one JSON line to standard output with the execution, step, attempt and `fields`. |

The context does not expose the contract version or `timeout_ms`.

## Cancellation and timeouts

When an attempt times out, or its workflow is cancelled or fails, the runtime
lists the attempt in the next heartbeat response and the SDK aborts
`ctx.signal`. Its `reason` is a non-retryable `TaskError` with code
`CANCELLED`. Pass the signal to APIs that accept one, or check it between
units of work:

```ts
worker.task("reports.fetch", { version: "1.0.0" }, async (input, ctx) => {
  const res = await fetch(input.url, { signal: ctx.signal });
  return { status: res.status };
});

worker.task("reports.render", { version: "1.0.0" }, async (input, ctx) => {
  for (const section of input.sections) {
    ctx.signal.throwIfAborted();
    await renderSection(section);
  }
  return {};
});
```

When an aborted `fetch` rejects, it rejects with the signal's reason, so the
attempt is reported as `CANCELLED`. The SDK does not abort the signal when
`deadlineMs` passes; only the runtime's cancellation does. Use
`AbortSignal.any([ctx.signal, AbortSignal.timeout(ctx.remainingMs() ?? 30_000)])`
to bound a call by the deadline as well.

## Concurrency

`concurrency` (default 4) is the number of poll loops, and therefore the
maximum number of tasks this worker runs at once. All loops share the Node.js
event loop, so handlers should be asynchronous. A CPU-bound handler blocks
every loop and the heartbeat; move that work to a `worker_threads` worker or
run more processes.

## Credentials and TLS

| Option | Environment | Default | Meaning |
|---|---|---|---|
| `server` | `ORBIT_SERVER` | `http://127.0.0.1:7233` | Runtime URL. |
| `token` | `ORBIT_WORKER_TOKEN`, then `ORBIT_TOKEN` | none | Bearer token. |
| `clientCert` | `ORBIT_CLIENT_CERT` | none | PEM client certificate for mutual TLS. |
| `clientKey` | `ORBIT_CLIENT_KEY` | the `clientCert` file | PEM private key. |

To trust a runtime whose certificate comes from a private CA, start Node.js
with `NODE_EXTRA_CA_CERTS=/path/to/ca.pem`. The global `fetch` cannot present
client certificates, so when `clientCert` is set and the server URL is
`https:`, the SDK sends requests through `node:https` instead. On that path
only, `ORBIT_CA_CERT` is read and replaces Node.js's default CA list.

```ts
const worker = new Worker({
  workerId: "payments-1",
  server: "https://orbit.internal:7233",
  clientCert: "/etc/orbit/payments-1.pem",
  clientKey: "/etc/orbit/payments-1.key",
});
```

```bash
ORBIT_CA_CERT=/etc/orbit/ca.pem node worker.ts
```

With a per-worker identity from a workers file, set `workerId` to the
identity's `id` and the token to the identity's token.

## Logging

SDK messages go to the `logger` option (default `console`), which needs
`info`, `warn` and `error` methods; messages are prefixed with `[orbit]`. Each
task execution is logged as a JSON line through `ctx.log`, which always writes
to `console.log`. To silence the SDK in tests, pass a logger with no-op
methods.

## Shutdown

`await worker.run()` resolves after `worker.stop()` is called or the process
receives `SIGINT` or `SIGTERM`. Shutdown aborts the open long polls, waits for
running handlers to finish and deliver their results, unregisters, removes the
signal listeners, and resolves. Pass `{ handleSignals: false }` to install
your own signal handling and call `stop()` yourself.

## Starting workflows

`Client` calls the runtime's HTTP API. Starting executions requires the admin
token, so pass it explicitly when the environment also holds a worker token:

```ts
import { Client } from "orbit-lang";

const client = new Client({ token: process.env.ORBIT_ADMIN_TOKEN });
const id = await client.start("DocumentInsights", { text: "Hello world" }, { executionId: "doc-42" });
const result = await client.wait(id, 60_000);
console.log(result.status, result.output);
```

### Signals and queries

`signal` sends a typed signal declared with `signal <name> { ... }` in the
workflow, and `query` evaluates a declared query. Both need the admin token
when authentication is enabled.

```ts
import { ApiError, Client } from "orbit-lang";

const client = new Client({ token: process.env.ORBIT_ADMIN_TOKEN });

type Status = { needs_approval: boolean; approved: boolean | null; reviewer: string | null; charged: boolean };
const status = await client.query<Status>("order-A-100", "status");
console.log(status.approved); // null while the review is pending

const seq = await client.signal("order-A-100", "review", { approved: true, reviewer: "dana" });

try {
  await client.signal("order-A-100", "review", { approved: "yes", reviewer: "dana" });
} catch (e) {
  if (e instanceof ApiError) console.log(e.code); // SIGNAL_PAYLOAD_INVALID
}
```

`signal` resolves to the history sequence number of the recorded
`SignalReceived` event. The runtime validates the payload and buffers the
signal until an `await` step takes it. See
[Signals and queries](../language/signals.md).

## API reference

### `Worker`

```ts
new Worker(options?: WorkerOptions)
```

`WorkerOptions` extends [`ClientOptions`](#client):

| Option | Type | Default | Meaning |
|---|---|---|---|
| `workerId` | `string` | `typescript-<pid>` | Stable identity sent at registration. |
| `concurrency` | `number` | `4` | Number of poll loops; values below 1 become 1. |
| `runtime` | `string` | `"typescript"` | Announced runtime. Set `"javascript"` for steps declared `using javascript`. |
| `pollWaitMs` | `number` | `20000` | `wait_ms` of each long poll. The runtime caps it at 60000. |
| `logger` | `Pick<Console, "info" \| "warn" \| "error">` | `console` | Destination of SDK messages. |

| Member | Meaning |
|---|---|
| `task<I, O>(name, { version? }, handler): this` | Registers `handler` for `name@version`. |
| `run({ handleSignals? }): Promise<void>` | Registers, polls and resolves after shutdown. `handleSignals` defaults to `true`. Rejects when no task is registered, or with `ApiError` when the runtime refuses registration; connection errors and 5xx responses are retried. |
| `stop(): void` | Starts a graceful shutdown. |
| `workerId: string` | The resolved worker id. |
| `client: Client` | The client used for every protocol call. |

### `TaskError`

```ts
new TaskError(code: string, message?: string, options?: { retryable?: boolean; details?: Json })
```

Read-only properties: `code`, `message`, `retryable`, `details`. `name` is
`"TaskError"`.

### `Context`

See [Context](#context). The SDK creates it; you do not construct one.

### `Client`

```ts
new Client(options?: ClientOptions)
```

| Option | Type | Default | Meaning |
|---|---|---|---|
| `server` | `string` | `ORBIT_SERVER`, then `http://127.0.0.1:7233` | Runtime URL. |
| `token` | `string` | `ORBIT_WORKER_TOKEN`, then `ORBIT_TOKEN` | Bearer token. |
| `clientCert` | `string` | `ORBIT_CLIENT_CERT` | Path of a PEM client certificate. |
| `clientKey` | `string` | `ORBIT_CLIENT_KEY`, then `clientCert` | Path of the PEM private key. |

| Method | Returns | Meaning |
|---|---|---|
| `request<T>(method, path, body?, signal?)` | `Promise<{ status: number; body: T }>` | Sends a JSON request. `body` is `null` for an empty response. Throws `ApiError` for status 400 and above. |
| `start(workflow, input = {}, { version?, executionId? })` | `Promise<string>` | Starts an execution and returns its id. |
| `wait(executionId, timeoutMs = 30000)` | `Promise<any>` | Long-polls until the execution finishes or the timeout elapses, and returns its state. |
| `signal(executionId, name, payload = {})` | `Promise<number>` | Sends the signal `name` and resolves to the history sequence number of the recorded signal. Rejects with `ApiError` (`UNKNOWN_SIGNAL`, `SIGNAL_PAYLOAD_INVALID`, `EXECUTION_FINISHED`, `NOT_FOUND`). |
| `query<T = any>(executionId, name)` | `Promise<T>` | Evaluates the query `name` and resolves to its value. `T` is not checked at runtime. Rejects with `ApiError` (`UNKNOWN_QUERY`, `QUERY_FAILED`, `NOT_FOUND`). |
| `server` | `string` | The resolved runtime URL. |

### `ApiError`

Thrown by `Client` for an error response or a connection failure. Properties:
`status` (`0` for a connection failure), `code` (the runtime's error code,
`CONNECTION`, or `HTTP_ERROR`), `message`, and the getter
`isConnectionError`.

### Other exports

| Export | Meaning |
|---|---|
| `SDK_VERSION` | `"0.1.0"`. |
| `Json` | Type of any JSON value. |
| `TaskAssignment` | Type of the task message the runtime sends (protocol `ExecuteTaskRequest`). |
| `Handler<I, O>` | `(input: I, ctx: Context) => O \| Promise<O>`. |
| `ClientOptions`, `WorkerOptions` | Option types described above. |

## Testing

Keep handlers as named functions so that you can call them directly. A plain
object can stand in for the context:

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { tokenize } from "./handlers.ts";

test("tokenize", async () => {
  const ctx = { signal: new AbortController().signal, log() {} } as any;
  assert.deepEqual(await tokenize({ text: "a b" }, ctx), { tokens: ["a", "b"] });
});
```

For an integration test, start `orbit server` with a temporary database,
deploy your workflow, call `worker.run({ handleSignals: false })` without
awaiting it, drive executions with `Client`, then call `worker.stop()` and
await the promise from `run()`. A short `pollWaitMs` keeps polls responsive.
The SDK's own test, `sdks/typescript/test/worker.test.ts`, runs the worker
against a scripted fake runtime built on `node:http`.

## See also

- [How workers work](overview.md)
- [Worker protocol](../reference/protocol.md)
- [Task contracts](../language/tasks.md)
- [Retries and timeouts](../language/retries-timeouts.md)
- [Idempotency](../language/idempotency.md)
- [Security](../runtime/security.md)
- [Polyglot workflows](../guides/polyglot.md)
