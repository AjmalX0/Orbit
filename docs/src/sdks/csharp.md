# C# and .NET

The C# SDK, the `OrbitLang.Sdk` NuGet package, runs ORBIT task handlers on
.NET. It depends only on the base class library (`System.Net.Http` and
`System.Text.Json`). Handlers are asynchronous and typed with
`System.Text.Json`, or work on raw `JsonElement` values, and each attempt gets
a `CancellationToken`. The SDK handles registration, long polling, heartbeats,
local deadlines, result delivery through runtime outages, and graceful
shutdown when the token passed to `RunAsync` is cancelled. Read
[How workers work](overview.md) first for the concepts this page relies on.

## Install

```bash
dotnet add package OrbitLang.Sdk
```

The package targets .NET 8.

## A minimal worker

This worker implements `greeting.csharp` from
[`examples/csharp_hello.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/csharp_hello.orbit):

```orbit
task "greeting.csharp" {
    version "1.0.0"
    input { name: String, excitement: Int }
    output { message: String, length: Int, dotnet_version: String }
}
```

```csharp
using System.Runtime.InteropServices;
using OrbitLang.Sdk;

using var worker = new Worker(new WorkerOptions { WorkerId = "hello-csharp" });

worker.Task<GreetInput, GreetOutput>("greeting.csharp", "1.0.0", async (input, ctx) =>
{
    if (string.IsNullOrWhiteSpace(input.Name))
        throw TaskException.Permanent("INVALID_NAME", "name must not be empty");
    if (input.Excitement is < 0 or > 10)
        throw TaskException.Permanent("INVALID_EXCITEMENT", "excitement must be between 0 and 10");
    await Task.Delay(10, ctx.CancellationToken);
    var message = $"Hello, {input.Name.Trim()}{new string('!', input.Excitement)}";
    ctx.Log($"composed greeting ({message.Length} chars)");
    return new GreetOutput(message, message.Length, Environment.Version.ToString());
});

using var stop = new CancellationTokenSource();
Console.CancelKeyPress += (_, e) => { e.Cancel = true; stop.Cancel(); };
using var sigterm = PosixSignalRegistration.Create(PosixSignal.SIGTERM, c => { c.Cancel = true; stop.Cancel(); });
await worker.RunAsync(stop.Token);

record GreetInput(string Name, int Excitement = 1);
record GreetOutput(string Message, int Length, string DotnetVersion);
```

The default JSON options use snake_case names, so `DotnetVersion` is written
as `dotnet_version`, matching the contract. Run the worker against a local
runtime; the repository's
[`examples/workers/csharp`](https://github.com/AjmalX0/Orbit/tree/main/examples/workers/csharp)
project contains the same worker:

```bash
orbit server &
dotnet run --project examples/workers/csharp &
orbit run examples/csharp_hello.orbit --input '{"name": "Ada", "excitement": 2}'
```

```json
{
  "message": "Hello, Ada!!",
  "length": 12
}
```

## Registering tasks

```csharp
Worker Task<TIn, TOut>(string name, string version, Func<TIn, TaskContext, Task<TOut>> handler)
Worker Task(string name, string version, Func<JsonElement, TaskContext, Task<object?>> handler)
```

Each overload registers a handler for `name@version` and returns the worker,
so calls can be chained. The name and version must match the `task`
declaration in your `.orbit` files exactly. Registering the same name twice,
or registering while the worker runs, throws `InvalidOperationException`.

## Handlers and results

The typed overload deserializes the input into `TIn` with the worker's
`JsonSerializerOptions`. The default options use
`JsonNamingPolicy.SnakeCaseLower` with case-insensitive reads, so `OrderId`
maps to `order_id`. Override them with `WorkerOptions.JsonOptions`. Input that
cannot be decoded fails the attempt with `INVALID_INPUT`, which is not
retried.

The raw overload receives the input as a `JsonElement` (`{}` when the runtime
sent none) and may return any JSON-serializable value, such as an anonymous
object, a `JsonObject`, or a `JsonElement`:

```csharp
worker.Task("inventory.release", "1.0.0", (input, ctx) =>
    Task.FromResult<object?>(new { released = input.GetProperty("reservation_id").GetString() }));
```

- Returning `null` reports `{}`.
- A result that does not serialize to a JSON object fails the attempt with
  `INVALID_RESULT`, which is not retried.
- The runtime validates the output against the contract's output schema; a
  mismatch fails the step with `OUTPUT_CONTRACT_VIOLATION`.

## Throwing task errors

Throw `TaskException` to fail an attempt with a structured error:

```csharp
throw TaskException.Permanent("CARD_DECLINED", "limit exceeded", new { limit = 5000 });
throw TaskException.Transient("BANK_UNAVAILABLE", "upstream timed out");
throw new TaskException("CARD_DECLINED", "limit exceeded", retryable: false);
```

| Constructor or factory | Retryable |
|---|---|
| `new TaskException(string code, string message, bool retryable = true, object? details = null)` | as given |
| `TaskException.Transient(string code, string message, object? details = null)` | yes |
| `TaskException.Permanent(string code, string message, object? details = null)` | no, fails the step whatever its retry policy |

`details` is serialized with the worker's JSON options and stored with the
error in the execution history. Any other exception is reported as a
retryable `UNHANDLED_EXCEPTION` whose message is `<Type>: <message>`, with
`details.type` and `details.stack`. The SDK also logs it.

## TaskContext

| Property or method | Type | Meaning |
|---|---|---|
| `Token` | `string` | Identifies this delivery. The SDK echoes it in the result. |
| `ExecutionId` | `string` | The workflow execution. |
| `Workflow` | `string` | Workflow name. |
| `WorkflowVersion` | `string` | Workflow version. |
| `StepId` | `string` | The step, or `compensate:<step>` for a compensation; `step[i]` for a fan-out element. |
| `TaskName` | `string` | The task name. |
| `ContractVersion` | `string` | The task's contract version. |
| `Input` | `JsonElement` | The raw input. |
| `IdempotencyKey` | `string` | Stable across retries and redeliveries. Use it for side effects. |
| `Attempt` | `int` | 1-based attempt number. Redeliveries keep the number. |
| `Redelivery` | `bool` | `true` when the outcome of an earlier delivery of this attempt is unknown. |
| `DeadlineMs` | `long?` | Absolute deadline in Unix epoch milliseconds, when the step has a `timeout`. |
| `TimeoutMs` | `long?` | The step's `timeout` in milliseconds. |
| `CancellationToken` | `CancellationToken` | Cancelled when the attempt should stop. |
| `Remaining` | `TimeSpan?` | Time left before `DeadlineMs`, never negative. |
| `Log(string message)` | `void` | Logs `message` prefixed with `[<execution>/<step>#<attempt>]`. |

## Cancellation and timeouts

`ctx.CancellationToken` is cancelled when any of these happens:

- the runtime lists the attempt in a heartbeat's `cancel` list, because the
  attempt timed out or its workflow was cancelled or failed;
- the attempt's `DeadlineMs` passes;
- the token passed to `RunAsync` is cancelled.

Pass the token to every asynchronous call that accepts one:

```csharp
worker.Task<FetchInput, FetchOutput>("reports.fetch", "1.0.0", async (input, ctx) =>
{
    using var response = await http.GetAsync(input.Url, ctx.CancellationToken);
    return new FetchOutput((int)response.StatusCode);
});
```

An `OperationCanceledException` that escapes the handler after the token was
cancelled is reported as a retryable `CANCELLED`, with a message that says
whether the runtime cancelled the attempt, the deadline passed, or the worker
is shutting down. Heartbeats run on their own task, so a long handler keeps
its session alive.

## Concurrency

`WorkerOptions.Concurrency` (default 4) is the number of poll loops, each
started with `Task.Run`, and therefore the maximum number of tasks this worker
runs at once. Handlers can run in parallel on thread-pool threads, so make
shared state thread-safe.

## Credentials and TLS

Every connection setting has a `WorkerOptions` property and an environment
variable. A non-empty property wins.

| Property | Environment | Default | Meaning |
|---|---|---|---|
| `Server` | `ORBIT_SERVER` | `http://127.0.0.1:7233` | Runtime URL. |
| `Token` | `ORBIT_WORKER_TOKEN`, then `ORBIT_TOKEN` | none | Bearer token. |
| `CaCertFile` | `ORBIT_CA_CERT` | none | PEM CA certificates, trusted in addition to the system store. |
| `ClientCertificatePath` | `ORBIT_CLIENT_CERT` | none | PEM client certificate for mutual TLS. |
| `ClientKeyPath` | `ORBIT_CLIENT_KEY` | the certificate file | PEM private key: PKCS#8, PKCS#1 or EC. |

```csharp
using var worker = new Worker(new WorkerOptions
{
    WorkerId = "payments-1",
    Server = "https://orbit.internal:7233",
    CaCertFile = "/etc/orbit/ca.pem",
    ClientCertificatePath = "/etc/orbit/payments-1.pem",
    ClientKeyPath = "/etc/orbit/payments-1.key",
});
```

The SDK loads the certificate with `X509Certificate2.CreateFromPemFile` and
re-imports it through PKCS#12, so that TLS stacks that reject ephemeral keys
accept it. A key without a certificate, a certificate without a key, or a CA
file without certificates throws from the constructor. With a per-worker
identity from a workers file, set `WorkerId` to the identity's `id` and the
token to the identity's token.

## Logging

SDK messages go to `WorkerOptions.Logger`, an `Action<string>`. The default
writes to standard error with the prefix `[orbit]` and a timestamp. To use
`Microsoft.Extensions.Logging`, pass a delegate:

```csharp
var options = new WorkerOptions { Logger = line => logger.LogInformation("{Line}", line) };
```

`ctx.Log` writes through the same delegate.

## Shutdown

`RunAsync` returns after the token passed to it is cancelled. It then aborts
the open long polls and cancels the `CancellationToken` of each running
handler. It waits for the handlers to finish and delivers their results
(retrying for up to 10 minutes), unregisters, and returns. The worker
implements `IDisposable`; dispose it after `RunAsync` returns.

A handler that ignores its token finishes its work and its result is
delivered. A handler that stops because of the token reports a retryable
`CANCELLED`, and that failure counts as an attempt. With the default retry
policy of one attempt, the step then fails. Give steps served by .NET workers
a retry policy with more than one attempt if you restart workers while they
run tasks.

## API reference

### `Worker`

```csharp
public sealed class Worker : IDisposable
public Worker(WorkerOptions? options = null)
```

| Member | Meaning |
|---|---|
| `Task<TIn, TOut>(name, version, handler)`, `Task(name, version, handler)` | Register handlers. See [Registering tasks](#registering-tasks). |
| `Task RunAsync(CancellationToken cancellationToken = default)` | Registers, polls and completes after shutdown. Throws `InvalidOperationException` when no task is registered or the worker is already running, and `OrbitApiException` when the runtime refuses registration. Connection errors and 5xx responses during registration are retried; cancelling the token while the runtime is unreachable returns without an error. |
| `void Dispose()` | Releases the HTTP client and the client certificate. |
| `WorkerId`, `Server`, `Runtime`, `Concurrency`, `PollWait` | The resolved settings. |
| `ClientCertificate` | The `X509Certificate2` presented for mutual TLS, or `null`. |
| `Version` | The SDK version, `"0.1.0"`. |

### `WorkerOptions`

| Property | Type | Default | Meaning |
|---|---|---|---|
| `WorkerId` | `string?` | `csharp-<machine>-<pid>` | Stable identity sent at registration. |
| `Server` | `string?` | `ORBIT_SERVER`, then `http://127.0.0.1:7233` | Runtime URL. |
| `Token` | `string?` | `ORBIT_WORKER_TOKEN`, then `ORBIT_TOKEN` | Bearer token. |
| `CaCertFile` | `string?` | `ORBIT_CA_CERT` | PEM CA certificate path. |
| `ClientCertificatePath` | `string?` | `ORBIT_CLIENT_CERT` | PEM client certificate path. |
| `ClientKeyPath` | `string?` | `ORBIT_CLIENT_KEY` | PEM private key path. |
| `Concurrency` | `int` | `4` | Number of poll loops; values below 1 become 1. |
| `Runtime` | `string` | `"csharp"` | Announced runtime, matched against `using <language>`. |
| `PollWait` | `TimeSpan` | 20 seconds | `wait_ms` of each long poll. The runtime caps it at 60 seconds. |
| `JsonOptions` | `JsonSerializerOptions?` | snake_case, case-insensitive | Options for input, output and details. |
| `Logger` | `Action<string>?` | standard error | Destination of SDK messages. |

### `TaskException`

Properties: `Code`, `Message`, `Retryable`, `Details`. See
[Throwing task errors](#throwing-task-errors).

### `OrbitApiException`

Thrown for an error response or a connection failure. Properties: `Status`
(`0` for a connection failure), `Code` (the runtime's error code,
`CONNECTION`, or `HTTP_ERROR`), `IsConnectionError`.

### `TaskContext`

See [TaskContext](#taskcontext). The SDK creates it; you do not construct one.

## Testing

Keep handler logic in methods you can call directly. The SDK constructs
`TaskContext`, so test the logic without it:

```csharp
static GreetOutput Greet(GreetInput input) => ...;

[Fact]
public void GreetsWithExcitement() =>
    Assert.Equal("Hello, Ada!!", Greet(new GreetInput("Ada", 2)).Message);
```

For an integration test, start `orbit server` with a temporary database,
deploy your workflow, start `worker.RunAsync(cts.Token)` without awaiting it,
drive executions through the HTTP API, then cancel `cts` and await the task.
A short `PollWait` keeps polls responsive, and a `Logger` that collects lines
lets you assert on SDK messages. The SDK's own tests in `sdks/csharp` run the
worker against a scripted fake runtime built on `HttpListener`, including
mutual TLS; run them with `dotnet test`.

## See also

- [How workers work](overview.md)
- [Worker protocol](../reference/protocol.md)
- [Task contracts](../language/tasks.md)
- [Retries and timeouts](../language/retries-timeouts.md)
- [Idempotency](../language/idempotency.md)
- [Security](../runtime/security.md)
- [Polyglot workflows](../guides/polyglot.md)
