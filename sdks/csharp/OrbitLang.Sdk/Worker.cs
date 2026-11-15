using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Net.Security;
using System.Security.Cryptography.X509Certificates;
using System.Text;
using System.Text.Json;
using System.Threading;
using SysTask = System.Threading.Tasks.Task;

namespace OrbitLang.Sdk;

/// <summary>
/// Runs task handlers for the ORBIT runtime. A worker announces the tasks it
/// implements, long-polls the runtime for work, runs the handler and reports the result.
/// </summary>
/// <example>
/// <code>
/// var worker = new Worker(new WorkerOptions { WorkerId = "inventory-csharp" });
/// worker.Task&lt;ReserveInput, ReserveOutput&gt;("inventory.reserve", "1.0.0", async (input, ctx) =>
///     new ReserveOutput($"res-{input.OrderId}"));
/// await worker.RunAsync(cancellationToken);
/// </code>
/// </example>
/// <remarks>
/// Delivery is at-least-once: a task can be delivered again after a crash
/// (<see cref="TaskContext.Redelivery"/>). Use <see cref="TaskContext.IdempotencyKey"/>,
/// which is stable across retries and redeliveries, to make side effects idempotent.
/// </remarks>
public sealed class Worker : IDisposable
{
    /// <summary>Version of this SDK.</summary>
    public const string Version = "0.1.0";

    internal const string DefaultServer = "http://127.0.0.1:7233";

    private static readonly JsonSerializerOptions ProtocolJson = new();
    private static readonly TimeSpan ResultDeliveryLimit = TimeSpan.FromMinutes(10);

    private readonly HttpClient _http;
    private readonly JsonSerializerOptions _json;
    private readonly Action<string> _log;
    private readonly Dictionary<string, Registered> _tasks = new();
    private readonly ConcurrentDictionary<string, (TaskContext Context, CancellationTokenSource Cts)> _active = new();
    private readonly SemaphoreSlim _registerLock = new(1, 1);
    private readonly object _gate = new();

    private string? _session;
    private TimeSpan _heartbeatEvery = TimeSpan.FromSeconds(5);
    private int _running;

    private sealed record Registered(string Version, Func<TaskContext, System.Threading.Tasks.Task<object?>> Handler);

    /// <summary>Creates a worker. Register tasks with <c>Task</c> before calling <see cref="RunAsync"/>.</summary>
    public Worker(WorkerOptions? options = null)
    {
        options ??= new WorkerOptions();
        Server = Resolve(options.Server, "ORBIT_SERVER") ?? DefaultServer;
        Server = Server.TrimEnd('/');
        Token = Resolve(options.Token, "ORBIT_WORKER_TOKEN") ?? Resolve(null, "ORBIT_TOKEN");
        CaCertFile = Resolve(options.CaCertFile, "ORBIT_CA_CERT");
        ClientCertificatePath = Resolve(options.ClientCertificatePath, "ORBIT_CLIENT_CERT");
        ClientKeyPath = Resolve(options.ClientKeyPath, "ORBIT_CLIENT_KEY");
        if (ClientKeyPath is not null && ClientCertificatePath is null)
            throw new InvalidOperationException("a client key was given (ClientKeyPath / ORBIT_CLIENT_KEY) without a client certificate");
        WorkerId = string.IsNullOrEmpty(options.WorkerId)
            ? $"csharp-{Environment.MachineName}-{Environment.ProcessId}"
            : options.WorkerId;
        Concurrency = Math.Max(1, options.Concurrency);
        Runtime = string.IsNullOrEmpty(options.Runtime) ? "csharp" : options.Runtime;
        PollWait = options.PollWait > TimeSpan.Zero ? options.PollWait : TimeSpan.FromSeconds(20);
        _json = options.JsonOptions ?? new JsonSerializerOptions
        {
            PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
            PropertyNameCaseInsensitive = true,
        };
        _log = options.Logger ?? (line => Console.Error.WriteLine($"[orbit] {DateTime.Now:yyyy-MM-dd HH:mm:ss} {line}"));

        var handler = new SocketsHttpHandler { PooledConnectionLifetime = TimeSpan.FromMinutes(5) };
        if (CaCertFile is not null)
        {
            var roots = LoadRoots(CaCertFile);
            handler.SslOptions.RemoteCertificateValidationCallback = (_, cert, chain, errors) => Validate(roots, cert, chain, errors);
        }
        if (ClientCertificatePath is not null)
        {
            ClientCertificate = LoadClientCertificate(ClientCertificatePath, ClientKeyPath);
            handler.SslOptions.ClientCertificates = new X509CertificateCollection { ClientCertificate };
        }
        Handler = handler;
        _http = new HttpClient(handler) { Timeout = PollWait + TimeSpan.FromSeconds(15) };
    }

    /// <summary>The worker identity sent on registration.</summary>
    public string WorkerId { get; }

    /// <summary>The runtime address.</summary>
    public string Server { get; }

    /// <summary>The announced runtime (<c>using &lt;language&gt;</c>).</summary>
    public string Runtime { get; }

    /// <summary>Number of concurrent poll loops.</summary>
    public int Concurrency { get; }

    /// <summary>Long-poll wait.</summary>
    public TimeSpan PollWait { get; }

    internal string? Token { get; }

    internal string? CaCertFile { get; }

    internal string? ClientCertificatePath { get; }

    internal string? ClientKeyPath { get; }

    /// <summary>The client certificate presented for mutual TLS, if configured.</summary>
    public X509Certificate2? ClientCertificate { get; }

    internal SocketsHttpHandler Handler { get; }

    private static string? Resolve(string? value, string env)
    {
        if (!string.IsNullOrEmpty(value)) return value;
        var fromEnv = Environment.GetEnvironmentVariable(env);
        return string.IsNullOrEmpty(fromEnv) ? null : fromEnv;
    }

    // ---------------------------------------------------------------- registration of handlers

    /// <summary>
    /// Registers a typed handler as the implementation of <paramref name="name"/>@<paramref name="version"/>.
    /// The input is deserialized into <typeparamref name="TIn"/> (an undecodable input fails the
    /// attempt with <c>INVALID_INPUT</c>, not retryable) and the result is serialized as the output,
    /// which must be a JSON object.
    /// </summary>
    public Worker Task<TIn, TOut>(string name, string version, Func<TIn, TaskContext, System.Threading.Tasks.Task<TOut>> handler)
    {
        ArgumentNullException.ThrowIfNull(handler);
        return Add(name, version, async ctx =>
        {
            TIn input;
            try
            {
                input = ctx.Input.Deserialize<TIn>(_json)!;
            }
            catch (Exception e) when (e is JsonException or NotSupportedException or InvalidOperationException)
            {
                throw new TaskException("INVALID_INPUT", $"cannot decode input as {typeof(TIn).Name}: {e.Message}", retryable: false);
            }
            return await handler(input, ctx).ConfigureAwait(false);
        });
    }

    /// <summary>
    /// Registers a raw handler as the implementation of <paramref name="name"/>@<paramref name="version"/>.
    /// It receives the input as a <see cref="JsonElement"/> and may return any JSON-serializable
    /// object (an anonymous object, a <c>JsonObject</c>, a <see cref="JsonElement"/>, ...).
    /// </summary>
    public Worker Task(string name, string version, Func<JsonElement, TaskContext, System.Threading.Tasks.Task<object?>> handler)
    {
        ArgumentNullException.ThrowIfNull(handler);
        return Add(name, version, ctx => handler(ctx.Input, ctx));
    }

    private Worker Add(string name, string version, Func<TaskContext, System.Threading.Tasks.Task<object?>> handler)
    {
        ArgumentException.ThrowIfNullOrEmpty(name);
        ArgumentException.ThrowIfNullOrEmpty(version);
        if (Volatile.Read(ref _running) != 0) throw new InvalidOperationException("tasks must be registered before RunAsync");
        if (!_tasks.TryAdd(name, new Registered(version, handler)))
            throw new InvalidOperationException($"task {name} is already registered");
        return this;
    }

    // ---------------------------------------------------------------- HTTP

    private async System.Threading.Tasks.Task<(int Status, JsonElement Body)> CallAsync(string path, object body, CancellationToken ct)
    {
        using var request = new HttpRequestMessage(HttpMethod.Post, Server + path)
        {
            Content = new StringContent(JsonSerializer.Serialize(body, body.GetType(), ProtocolJson), Encoding.UTF8, "application/json"),
        };
        if (Token is not null) request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", Token);
        int status;
        string raw;
        try
        {
            using var response = await _http.SendAsync(request, ct).ConfigureAwait(false);
            status = (int)response.StatusCode;
            raw = await response.Content.ReadAsStringAsync(ct).ConfigureAwait(false);
        }
        catch (OperationCanceledException) when (ct.IsCancellationRequested)
        {
            throw;
        }
        catch (Exception e) when (e is HttpRequestException or IOException or OperationCanceledException)
        {
            // OperationCanceledException here is HttpClient's own timeout.
            throw new OrbitApiException(0, "CONNECTION", $"cannot reach ORBIT runtime at {Server}: {e.Message}", e);
        }
        if (status >= 400)
        {
            string code = "HTTP_ERROR", message = raw;
            try
            {
                using var doc = JsonDocument.Parse(raw);
                if (doc.RootElement.ValueKind == JsonValueKind.Object
                    && doc.RootElement.TryGetProperty("error", out var err)
                    && err.ValueKind == JsonValueKind.Object
                    && err.TryGetProperty("code", out var c) && c.ValueKind == JsonValueKind.String)
                {
                    code = c.GetString()!;
                    message = err.TryGetProperty("message", out var m) && m.ValueKind == JsonValueKind.String ? m.GetString()! : "";
                }
            }
            catch (JsonException)
            {
                // not a structured error body
            }
            throw new OrbitApiException(status, code, message);
        }
        if (string.IsNullOrWhiteSpace(raw)) return (status, default);
        using (var doc = JsonDocument.Parse(raw)) return (status, doc.RootElement.Clone());
    }

    private static T? Read<T>(JsonElement body) where T : class =>
        body.ValueKind == JsonValueKind.Object ? body.Deserialize<T>(ProtocolJson) : null;

    // ---------------------------------------------------------------- session

    private async System.Threading.Tasks.Task<string> RegisterAsync(string? stale, CancellationToken ct)
    {
        await _registerLock.WaitAsync(ct).ConfigureAwait(false);
        try
        {
            var current = Volatile.Read(ref _session);
            if (current is not null && current != stale) return current; // another loop already re-registered
            var contracts = _tasks.Select(t => new TaskContract(t.Key, t.Value.Version)).ToList();
            var (_, body) = await CallAsync("/v1/workers/register",
                new RegisterWorkerRequest(WorkerId, Runtime, "orbit-csharp/" + Version, contracts), ct).ConfigureAwait(false);
            var r = Read<RegisterWorkerResponse>(body) ?? new RegisterWorkerResponse();
            if (string.IsNullOrEmpty(r.WorkerSessionId))
                throw new OrbitApiException(200, "REGISTRATION_REJECTED", "the runtime did not return a worker session");
            lock (_gate)
            {
                _session = r.WorkerSessionId;
                if (r.HeartbeatIntervalMs > 0) _heartbeatEvery = TimeSpan.FromMilliseconds(Math.Max(r.HeartbeatIntervalMs, 500));
            }
            foreach (var warning in r.Warnings ?? []) _log($"runtime: {warning}");
            _log($"worker {WorkerId} registered ({_tasks.Count} tasks)");
            return r.WorkerSessionId;
        }
        finally
        {
            _registerLock.Release();
        }
    }

    /// <summary>Runs <paramref name="fn"/> with the current session, re-registering once if the runtime forgot it.</summary>
    private async System.Threading.Tasks.Task<T> WithSessionAsync<T>(Func<string, System.Threading.Tasks.Task<T>> fn, CancellationToken ct)
    {
        var session = Volatile.Read(ref _session) ?? await RegisterAsync(null, ct).ConfigureAwait(false);
        try
        {
            return await fn(session).ConfigureAwait(false);
        }
        catch (OrbitApiException e) when (e.Status == 404 && e.Code == "UNKNOWN_SESSION")
        {
            session = await RegisterAsync(session, ct).ConfigureAwait(false);
            return await fn(session).ConfigureAwait(false);
        }
    }

    /// <summary>
    /// Calls <paramref name="fn"/> until it succeeds, fails with a non-retryable error, or
    /// <paramref name="stop"/> is cancelled. Connection errors and 5xx responses are retried with backoff.
    /// </summary>
    private async System.Threading.Tasks.Task<T> RetryingAsync<T>(Func<System.Threading.Tasks.Task<T>> fn, CancellationToken stop)
    {
        var delay = TimeSpan.FromMilliseconds(200);
        while (true)
        {
            try
            {
                return await fn().ConfigureAwait(false);
            }
            catch (OrbitApiException e) when (e.IsConnectionError || e.Status >= 500)
            {
                if (stop.IsCancellationRequested)
                {
                    _log($"{e.Message}; giving up because the worker is stopping");
                    throw;
                }
                _log($"{e.Message}; retrying in {delay.TotalMilliseconds:0}ms");
                await DelayAsync(delay, stop).ConfigureAwait(false);
                var next = Math.Min(delay.TotalMilliseconds * 2, 5000);
                delay = TimeSpan.FromMilliseconds(next * (0.8 + Random.Shared.NextDouble() * 0.4));
            }
        }
    }

    private static async System.Threading.Tasks.Task<bool> DelayAsync(TimeSpan delay, CancellationToken ct)
    {
        try
        {
            await SysTask.Delay(delay, ct).ConfigureAwait(false);
            return true;
        }
        catch (OperationCanceledException)
        {
            return false;
        }
    }

    // ---------------------------------------------------------------- run loop

    /// <summary>
    /// Polls for work until <paramref name="cancellationToken"/> is cancelled, then waits for
    /// in-flight tasks to finish (their results are still delivered) and unregisters.
    /// </summary>
    /// <exception cref="InvalidOperationException">No task is registered, or the worker is already running.</exception>
    /// <exception cref="OrbitApiException">The runtime rejected the registration.</exception>
    public async SysTask RunAsync(CancellationToken cancellationToken = default)
    {
        if (_tasks.Count == 0) throw new InvalidOperationException("no tasks registered");
        if (Interlocked.Exchange(ref _running, 1) != 0) throw new InvalidOperationException("the worker is already running");
        try
        {
            try
            {
                await RetryingAsync(() => RegisterAsync(null, cancellationToken), cancellationToken).ConfigureAwait(false);
            }
            catch (Exception e) when (cancellationToken.IsCancellationRequested && e is OperationCanceledException or OrbitApiException)
            {
                return; // stopped before the runtime was reachable
            }

            var loops = new List<SysTask> { SysTask.Run(() => HeartbeatLoopAsync(cancellationToken), CancellationToken.None) };
            for (var i = 0; i < Concurrency; i++)
                loops.Add(SysTask.Run(() => PollLoopAsync(cancellationToken), CancellationToken.None));
            await SysTask.WhenAll(loops).ConfigureAwait(false);

            var session = Volatile.Read(ref _session);
            if (session is not null)
            {
                try
                {
                    using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(10));
                    await CallAsync("/v1/workers/unregister", new SessionRequest(session), timeout.Token).ConfigureAwait(false);
                }
                catch (Exception e) when (e is OrbitApiException or OperationCanceledException or JsonException)
                {
                    _log($"unregister failed: {e.Message}");
                }
            }
            _log($"worker {WorkerId} stopped");
        }
        finally
        {
            Volatile.Write(ref _running, 0);
        }
    }

    private async SysTask HeartbeatLoopAsync(CancellationToken ct)
    {
        while (true)
        {
            TimeSpan every;
            lock (_gate) every = _heartbeatEvery;
            if (!await DelayAsync(every, ct).ConfigureAwait(false)) return;
            HeartbeatResponse? r;
            try
            {
                var (_, body) = await WithSessionAsync(s => CallAsync("/v1/workers/heartbeat", new SessionRequest(s), ct), ct)
                    .ConfigureAwait(false);
                r = Read<HeartbeatResponse>(body);
            }
            catch (OperationCanceledException) when (ct.IsCancellationRequested)
            {
                return;
            }
            catch (Exception e) when (e is OrbitApiException or JsonException)
            {
                continue; // the poll loops report connectivity problems
            }
            foreach (var token in r?.Cancel ?? [])
            {
                if (_active.TryGetValue(token, out var entry))
                {
                    _log($"cancellation requested for {token[..Math.Min(8, token.Length)]}");
                    entry.Context.MarkCancelled("attempt cancelled by the runtime");
                    try { entry.Cts.Cancel(); } catch (ObjectDisposedException) { /* finished meanwhile */ }
                }
            }
        }
    }

    private async SysTask PollLoopAsync(CancellationToken ct)
    {
        var backoff = TimeSpan.FromMilliseconds(200);
        while (!ct.IsCancellationRequested)
        {
            ExecuteTaskRequest? task;
            try
            {
                var (status, body) = await WithSessionAsync(
                    s => CallAsync("/v1/workers/poll", new PollTaskRequest(s, (long)PollWait.TotalMilliseconds), ct), ct).ConfigureAwait(false);
                task = status == 204 ? null : Read<ExecuteTaskRequest>(body);
            }
            catch (OperationCanceledException) when (ct.IsCancellationRequested)
            {
                return;
            }
            catch (Exception e) when (e is OrbitApiException or JsonException)
            {
                if (ct.IsCancellationRequested) return;
                _log($"poll failed: {e.Message}; retrying in {backoff.TotalMilliseconds:0}ms");
                await DelayAsync(backoff, ct).ConfigureAwait(false);
                backoff = TimeSpan.FromMilliseconds(Math.Min(backoff.TotalMilliseconds * 2, 5000));
                continue;
            }
            backoff = TimeSpan.FromMilliseconds(200);
            if (task is null || string.IsNullOrEmpty(task.Token)) continue;
            await ExecuteAsync(task, ct).ConfigureAwait(false);
        }
    }

    // ---------------------------------------------------------------- execution

    private async SysTask ExecuteAsync(ExecuteTaskRequest task, CancellationToken stopping)
    {
        using var cts = CancellationTokenSource.CreateLinkedTokenSource(stopping);
        if (task.DeadlineMs is long deadline)
        {
            var remaining = deadline - DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
            if (remaining <= 0) cts.Cancel();
            else cts.CancelAfter(TimeSpan.FromMilliseconds(Math.Min(remaining, int.MaxValue - 1)));
        }
        var input = task.Input.ValueKind is JsonValueKind.Undefined or JsonValueKind.Null ? EmptyObject() : task.Input;
        var ctx = new TaskContext(task, input, cts.Token, _log);
        _active[task.Token] = (ctx, cts);
        try
        {
            TaskException failure;
            try
            {
                var output = await InvokeAsync(ctx).ConfigureAwait(false);
                if (TrySerializeOutput(output, out var json))
                {
                    await DeliverAsync("/v1/tasks/complete", new CompleteTaskRequest(task.Token, json)).ConfigureAwait(false);
                    return;
                }
                failure = new TaskException("INVALID_RESULT", "tasks must return a JSON object", retryable: false);
            }
            catch (TaskException e)
            {
                failure = e;
            }
            catch (OperationCanceledException e) when (cts.IsCancellationRequested)
            {
                var reason = ctx.CancelReason
                    ?? (stopping.IsCancellationRequested ? "the worker is shutting down" : "the attempt deadline passed");
                failure = new TaskException("CANCELLED", reason, retryable: true, new { type = e.GetType().FullName });
            }
            catch (Exception e)
            {
                _log($"task {task.TaskName} threw {e}");
                failure = new TaskException("UNHANDLED_EXCEPTION", $"{e.GetType().Name}: {e.Message}", retryable: true,
                    new { type = e.GetType().FullName, stack = e.ToString() });
            }
            await DeliverAsync("/v1/tasks/fail", new FailTaskRequest(task.Token, failure.Code, failure.Message, failure.Retryable,
                SerializeDetails(failure.Details))).ConfigureAwait(false);
        }
        finally
        {
            _active.TryRemove(new KeyValuePair<string, (TaskContext, CancellationTokenSource)>(task.Token, (ctx, cts)));
        }
    }

    private System.Threading.Tasks.Task<object?> InvokeAsync(TaskContext ctx)
    {
        if (!_tasks.TryGetValue(ctx.TaskName, out var impl))
            throw new TaskException("UNKNOWN_TASK", $"worker does not implement {ctx.TaskName}", retryable: true);
        return impl.Handler(ctx);
    }

    private bool TrySerializeOutput(object? output, out JsonElement json)
    {
        json = default;
        if (output is null)
        {
            json = EmptyObject();
            return true;
        }
        try
        {
            json = output is JsonElement element ? element.Clone() : JsonSerializer.SerializeToElement(output, output.GetType(), _json);
        }
        catch (Exception e) when (e is JsonException or NotSupportedException or InvalidOperationException)
        {
            _log($"cannot serialize task result: {e.Message}");
            return false;
        }
        return json.ValueKind == JsonValueKind.Object;
    }

    private JsonElement? SerializeDetails(object? details)
    {
        if (details is null) return null;
        try
        {
            return details is JsonElement element ? element.Clone() : JsonSerializer.SerializeToElement(details, details.GetType(), _json);
        }
        catch (Exception e) when (e is JsonException or NotSupportedException or InvalidOperationException)
        {
            return null;
        }
    }

    private static JsonElement EmptyObject()
    {
        using var doc = JsonDocument.Parse("{}");
        return doc.RootElement.Clone();
    }

    /// <summary>
    /// Reports a result, retrying through outages (connection errors and 5xx) so that it
    /// survives a runtime restart: the runtime reconciles results for unresolved steps.
    /// <c>accepted: false</c> is final.
    /// </summary>
    private async SysTask DeliverAsync(string path, object body)
    {
        using var stop = new CancellationTokenSource(ResultDeliveryLimit);
        try
        {
            var (_, response) = await RetryingAsync(() => CallAsync(path, body, CancellationToken.None), stop.Token).ConfigureAwait(false);
            var ack = Read<TaskResultAck>(response);
            if (ack is not null && !ack.Accepted) _log($"runtime ignored result: {ack.Reason}");
        }
        catch (Exception e) when (e is OrbitApiException or JsonException)
        {
            _log($"could not deliver result: {e.Message}");
        }
    }

    // ---------------------------------------------------------------- TLS

    private static X509Certificate2Collection LoadRoots(string path)
    {
        var roots = new X509Certificate2Collection();
        roots.ImportFromPemFile(path);
        if (roots.Count == 0) throw new InvalidOperationException($"no certificate found in {path}");
        return roots;
    }

    /// <summary>
    /// Loads a PEM certificate and its private key (from <paramref name="keyPath"/>, or from the
    /// certificate file itself when it is null).
    /// </summary>
    internal static X509Certificate2 LoadClientCertificate(string certPath, string? keyPath)
    {
        using var pem = X509Certificate2.CreateFromPemFile(certPath, keyPath);
        if (!pem.HasPrivateKey) throw new InvalidOperationException($"no private key found for client certificate {certPath}");
        // A key loaded from PEM is ephemeral. Some TLS stacks (SChannel on Windows, and some
        // OpenSSL configurations on Linux) reject an ephemeral key during the handshake, so
        // re-import it through PKCS#12, which works on every platform.
        return new X509Certificate2(pem.Export(X509ContentType.Pkcs12));
    }

    /// <summary>Accepts certificates trusted by the system or issued by one of <paramref name="roots"/>.</summary>
    internal static bool Validate(X509Certificate2Collection roots, X509Certificate? certificate, X509Chain? chain, SslPolicyErrors errors)
    {
        if (errors == SslPolicyErrors.None) return true;
        if (certificate is null || (errors & SslPolicyErrors.RemoteCertificateNameMismatch) != 0) return false;
        using var custom = new X509Chain();
        custom.ChainPolicy.TrustMode = X509ChainTrustMode.CustomRootTrust;
        custom.ChainPolicy.RevocationMode = X509RevocationMode.NoCheck;
        custom.ChainPolicy.CustomTrustStore.AddRange(roots);
        if (chain is not null)
        {
            foreach (var element in chain.ChainElements) custom.ChainPolicy.ExtraStore.Add(element.Certificate);
        }
        var leaf = certificate as X509Certificate2 ?? new X509Certificate2(certificate);
        return custom.Build(leaf);
    }

    /// <summary>Releases the HTTP client.</summary>
    public void Dispose()
    {
        _http.Dispose();
        ClientCertificate?.Dispose();
        _registerLock.Dispose();
    }
}
