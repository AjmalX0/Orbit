// C# worker for examples/csharp_hello.orbit.
//
// Run:  dotnet run --project examples/workers/csharp
using System.Runtime.InteropServices;
using OrbitLang.Sdk;

using var worker = new Worker(new WorkerOptions
{
    WorkerId = Environment.GetEnvironmentVariable("ORBIT_WORKER_ID") ?? "hello-csharp",
});

worker.Task<GreetInput, GreetOutput>("greeting.csharp", "1.0.0", async (input, ctx) =>
{
    if (string.IsNullOrWhiteSpace(input.Name))
        throw TaskException.Permanent("INVALID_NAME", "name must not be empty");
    if (input.Excitement is < 0 or > 10)
        throw TaskException.Permanent("INVALID_EXCITEMENT", "excitement must be between 0 and 10");
    await Task.Yield();
    var message = $"Hello, {input.Name.Trim()}{new string('!', input.Excitement)}";
    ctx.Log($"composed greeting ({message.Length} chars)");
    return new GreetOutput(message, message.Length, Environment.Version.ToString());
});

using var stop = new CancellationTokenSource();
Console.CancelKeyPress += (_, e) => { e.Cancel = true; stop.Cancel(); };
using var sigterm = PosixSignalRegistration.Create(PosixSignal.SIGTERM, ctx => { ctx.Cancel = true; stop.Cancel(); });
await worker.RunAsync(stop.Token);

internal sealed record GreetInput(string Name, int Excitement = 1);

internal sealed record GreetOutput(string Message, int Length, string DotnetVersion);
