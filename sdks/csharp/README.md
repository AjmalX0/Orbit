# ORBIT C# SDK

Worker SDK for the [ORBIT](https://github.com/AjmalX0/Orbit) runtime. .NET 8+, no
dependencies beyond the base class library (`System.Net.Http`, `System.Text.Json`).

```csharp
using OrbitLang.Sdk;

using var worker = new Worker(new WorkerOptions { WorkerId = "inventory-csharp", Concurrency = 8 });
worker.Task<ReserveInput, ReserveOutput>("inventory.reserve", "1.0.0", async (input, ctx) =>
{
    if (input.Items.Count == 0) throw TaskException.Permanent("EMPTY_ORDER", "order has no items");
    // ctx.IdempotencyKey is stable across retries and redeliveries.
    await Task.Delay(10, ctx.CancellationToken);
    return new ReserveOutput($"res-{input.OrderId}");
});

using var stop = new CancellationTokenSource();
Console.CancelKeyPress += (_, e) => { e.Cancel = true; stop.Cancel(); };
await worker.RunAsync(stop.Token); // returns after in-flight tasks finish and the worker unregisters

record LineItem(string Sku, int Quantity);
record ReserveInput(string OrderId, List<LineItem> Items);
record ReserveOutput(string ReservationId);
```

* `worker.Task<TIn, TOut>(name, version, handler)` decodes the input with
  `System.Text.Json` (snake_case names by default, so `OrderId` is `order_id`;
  override with `WorkerOptions.JsonOptions`). An input that cannot be decoded
  fails the attempt with `INVALID_INPUT` (not retryable). The raw variant
  `worker.Task(name, version, async (JsonElement input, TaskContext ctx) => ...)`
  may return any JSON-serializable object.
* The result must serialize to a JSON object (`null` means `{}`). Anything else
  fails with `INVALID_RESULT` (not retryable).
* `ctx.CancellationToken` is cancelled when the runtime cancels the attempt
  (timeout or workflow cancellation), when the attempt's deadline passes, or
  when the worker shuts down. An `OperationCanceledException` that escapes the
  handler is reported as retryable `CANCELLED`.
* Throw `TaskException(code, message, retryable, details)` (or
  `TaskException.Permanent` / `TaskException.Transient`) for business errors.
  Other exceptions are reported as retryable `UNHANDLED_EXCEPTION`, with the
  stack trace in `details`.
* Results are delivered through runtime outages (connection errors and 5xx are
  retried with backoff), and the worker re-registers when the runtime forgets
  its session.
* Configuration comes from `WorkerOptions` or from `ORBIT_SERVER`,
  `ORBIT_WORKER_TOKEN` / `ORBIT_TOKEN` and `ORBIT_CA_CERT`.
* Example: [`examples/workers/csharp`](../../examples/workers/csharp).
* Build, test and pack (from `sdks/csharp`): `dotnet build`, `dotnet test`,
  `dotnet pack OrbitLang.Sdk -c Release`
