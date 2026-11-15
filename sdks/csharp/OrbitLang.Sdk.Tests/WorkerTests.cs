using System.Net.Security;
using System.Security.Cryptography;
using System.Security.Cryptography.X509Certificates;
using System.Text.Json;
using System.Text.Json.Nodes;
using Xunit;

namespace OrbitLang.Sdk.Tests;

public sealed class WorkerTests
{
    private sealed record EchoIn(int N);

    private sealed record EchoOut(int N);

    private static async Task WaitFor(Func<bool> condition)
    {
        var deadline = DateTime.UtcNow.AddSeconds(15);
        while (!condition())
        {
            if (DateTime.UtcNow > deadline) throw new TimeoutException("condition not reached");
            await Task.Delay(10);
        }
    }

    [Fact]
    public async Task WorkerProtocol()
    {
        using var f = new FakeRuntime();
        lock (f.Lock)
        {
            f.ForgetOnce = true;
            f.FailComplete = 2;
            f.Tasks.AddRange(new[]
            {
                FakeRuntime.MakeTask("ok", "echo", new { n = 1 }),
                FakeRuntime.MakeTask("biz", "fail", null),
                FakeRuntime.MakeTask("boom", "throw", null),
                FakeRuntime.MakeTask("slow", "slow", null),
                FakeRuntime.MakeTask("bad", "array", null),
                FakeRuntime.MakeTask("raw", "raw", new { word = "orbit" }),
                FakeRuntime.MakeTask("late", "late", null, DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() + 300),
            });
        }

        var logs = new List<string>();
        using var worker = new Worker(new WorkerOptions
        {
            Server = f.Url,
            Token = "wt",
            WorkerId = "csharp-test",
            Concurrency = 2,
            PollWait = TimeSpan.FromMilliseconds(100),
            Logger = line => { lock (logs) logs.Add(line); },
        });
        worker.Task<EchoIn, EchoOut>("echo", "1.0.0", (input, ctx) => Task.FromResult(new EchoOut(input.N + 1)));
        worker.Task<JsonElement, object>("fail", "1.0.0", (_, _) => throw TaskException.Permanent("CARD_DECLINED", "no funds"));
        worker.Task("throw", "1.0.0", async (_, _) =>
        {
            await Task.Yield();
            throw new InvalidOperationException("kaboom");
        });
        worker.Task("slow", "1.0.0", async (_, ctx) =>
        {
            lock (f.Lock) f.Cancel = new List<string> { ctx.Token };
            try
            {
                await Task.Delay(Timeout.Infinite, ctx.CancellationToken); // released by the heartbeat's cancel list
            }
            catch (OperationCanceledException)
            {
            }
            throw TaskException.Permanent("CANCELLED", "stopped");
        });
        worker.Task("array", "1.0.0", (_, _) => Task.FromResult<object?>(new[] { 1, 2, 3 }));
        worker.Task("raw", "1.0.0", (input, ctx) =>
            Task.FromResult<object?>(new { upper = input.GetProperty("word").GetString()!.ToUpperInvariant(), key = ctx.IdempotencyKey }));
        worker.Task("late", "1.0.0", async (_, ctx) =>
        {
            await Task.Delay(Timeout.Infinite, ctx.CancellationToken); // released by deadline_ms
            return new { };
        });

        using var stop = new CancellationTokenSource();
        var run = worker.RunAsync(stop.Token);

        await WaitFor(() => { lock (f.Lock) return f.Completed.Count == 2 && f.Failed.Count == 5; });
        stop.Cancel();
        await run.WaitAsync(TimeSpan.FromSeconds(10));

        lock (f.Lock)
        {
            Assert.Equal("""{"n":2}""", f.Completed["ok"]!.ToJsonString());
            Assert.Equal("""{"upper":"ORBIT","key":"e/raw"}""", f.Completed["raw"]!.ToJsonString());
            Assert.True(f.Registrations >= 2, $"expected re-registration after UNKNOWN_SESSION, got {f.Registrations}");

            var reg = f.LastRegistration!;
            Assert.Equal("csharp-test", (string?)reg["worker_id"]);
            Assert.Equal("csharp", (string?)reg["runtime"]);
            Assert.Equal("orbit-csharp/0.1.0", (string?)reg["sdk"]);
            Assert.Equal(7, reg["contracts"]!.AsArray().Count);
            Assert.Contains(reg["contracts"]!.AsArray(),
                c => (string?)c!["task_name"] == "echo" && (string?)c["contract_version"] == "1.0.0");

            var biz = f.Failed["biz"];
            Assert.Equal("CARD_DECLINED", (string?)biz["error_code"]);
            Assert.Equal("no funds", (string?)biz["error_message"]);
            Assert.False((bool)biz["retryable"]!);

            var boom = f.Failed["boom"];
            Assert.Equal("UNHANDLED_EXCEPTION", (string?)boom["error_code"]);
            Assert.True((bool)boom["retryable"]!);
            Assert.Contains("kaboom", (string?)boom["error_message"]);
            Assert.Contains("InvalidOperationException", (string?)boom["details"]!["stack"]);
            Assert.Contains("WorkerTests", (string?)boom["details"]!["stack"]);

            Assert.Equal("CANCELLED", (string?)f.Failed["slow"]["error_code"]);
            Assert.Equal("stopped", (string?)f.Failed["slow"]["error_message"]);

            var bad = f.Failed["bad"];
            Assert.Equal("INVALID_RESULT", (string?)bad["error_code"]);
            Assert.False((bool)bad["retryable"]!);

            var late = f.Failed["late"];
            Assert.Equal("CANCELLED", (string?)late["error_code"]);
            Assert.Equal("the attempt deadline passed", (string?)late["error_message"]);
            Assert.True((bool)late["retryable"]!);

            Assert.Equal("Bearer wt", f.AuthHeader);
            Assert.Equal(1, f.Unregistrations);
            Assert.Equal(0, f.FailComplete); // both 503s were retried through
        }
    }

    [Fact]
    public async Task InvalidInputIsPermanent()
    {
        using var f = new FakeRuntime();
        lock (f.Lock) f.Tasks.Add(FakeRuntime.MakeTask("t", "echo", new { n = "not a number" }));
        using var worker = new Worker(new WorkerOptions
        {
            Server = f.Url, PollWait = TimeSpan.FromMilliseconds(100), Logger = _ => { },
        });
        worker.Task<EchoIn, EchoOut>("echo", "1.0.0", (input, _) => Task.FromResult(new EchoOut(input.N)));
        using var stop = new CancellationTokenSource();
        var run = worker.RunAsync(stop.Token);
        await WaitFor(() => { lock (f.Lock) return f.Failed.Count == 1; });
        stop.Cancel();
        await run.WaitAsync(TimeSpan.FromSeconds(10));
        lock (f.Lock)
        {
            Assert.Equal("INVALID_INPUT", (string?)f.Failed["t"]["error_code"]);
            Assert.False((bool)f.Failed["t"]["retryable"]!);
        }
    }

    [Fact]
    public async Task RunWithoutTasksFails()
    {
        using var worker = new Worker(new WorkerOptions { Server = "http://127.0.0.1:1", Logger = _ => { } });
        await Assert.ThrowsAsync<InvalidOperationException>(() => worker.RunAsync());
    }

    [Fact]
    public void DuplicateTaskIsRejected()
    {
        using var worker = new Worker(new WorkerOptions { Server = "http://127.0.0.1:1", Logger = _ => { } });
        worker.Task("a", "1.0.0", (_, _) => Task.FromResult<object?>(null));
        Assert.Throws<InvalidOperationException>(() => worker.Task("a", "1.0.0", (_, _) => Task.FromResult<object?>(null)));
    }

    [Fact]
    public void ConfigurationFromEnvironment()
    {
        string?[] saved =
        [
            Environment.GetEnvironmentVariable("ORBIT_SERVER"),
            Environment.GetEnvironmentVariable("ORBIT_WORKER_TOKEN"),
            Environment.GetEnvironmentVariable("ORBIT_TOKEN"),
        ];
        try
        {
            Environment.SetEnvironmentVariable("ORBIT_SERVER", "http://orbit.example:7000/");
            Environment.SetEnvironmentVariable("ORBIT_WORKER_TOKEN", null);
            Environment.SetEnvironmentVariable("ORBIT_TOKEN", "admin");
            using (var w = new Worker())
            {
                Assert.Equal("http://orbit.example:7000", w.Server);
                Assert.Equal("admin", w.Token);
                Assert.Equal("csharp", w.Runtime);
                Assert.Equal(4, w.Concurrency);
                Assert.Equal(TimeSpan.FromSeconds(20), w.PollWait);
                Assert.StartsWith("csharp-", w.WorkerId);
            }
            Environment.SetEnvironmentVariable("ORBIT_WORKER_TOKEN", "worker");
            using (var w = new Worker()) Assert.Equal("worker", w.Token);
            using (var w = new Worker(new WorkerOptions { Token = "explicit", Server = "http://x" }))
            {
                Assert.Equal("explicit", w.Token);
                Assert.Equal("http://x", w.Server);
            }
            Environment.SetEnvironmentVariable("ORBIT_SERVER", null);
            using (var w = new Worker()) Assert.Equal("http://127.0.0.1:7233", w.Server);
        }
        finally
        {
            Environment.SetEnvironmentVariable("ORBIT_SERVER", saved[0]);
            Environment.SetEnvironmentVariable("ORBIT_WORKER_TOKEN", saved[1]);
            Environment.SetEnvironmentVariable("ORBIT_TOKEN", saved[2]);
        }
    }

    [Fact]
    public void CustomCaValidation()
    {
        using var caKey = RSA.Create(2048);
        var caReq = new CertificateRequest("CN=Orbit Test CA", caKey, HashAlgorithmName.SHA256, RSASignaturePadding.Pkcs1);
        caReq.CertificateExtensions.Add(new X509BasicConstraintsExtension(true, false, 0, true));
        caReq.CertificateExtensions.Add(new X509KeyUsageExtension(X509KeyUsageFlags.KeyCertSign | X509KeyUsageFlags.CrlSign, true));
        using var ca = caReq.CreateSelfSigned(DateTimeOffset.UtcNow.AddDays(-1), DateTimeOffset.UtcNow.AddDays(30));

        using var leafKey = RSA.Create(2048);
        var leafReq = new CertificateRequest("CN=localhost", leafKey, HashAlgorithmName.SHA256, RSASignaturePadding.Pkcs1);
        leafReq.CertificateExtensions.Add(new X509BasicConstraintsExtension(false, false, 0, false));
        using var leaf = leafReq.Create(ca, DateTimeOffset.UtcNow.AddDays(-1), DateTimeOffset.UtcNow.AddDays(10), [1, 2, 3, 4]);

        var pem = Path.Combine(Path.GetTempPath(), $"orbit-ca-{Guid.NewGuid():N}.pem");
        File.WriteAllText(pem, ca.ExportCertificatePem());
        try
        {
            var roots = new X509Certificate2Collection();
            roots.ImportFromPemFile(pem);
            Assert.True(Worker.Validate(roots, leaf, null, SslPolicyErrors.RemoteCertificateChainErrors));
            Assert.False(Worker.Validate(roots, leaf, null, SslPolicyErrors.RemoteCertificateNameMismatch));

            using var otherKey = RSA.Create(2048);
            using var other = new CertificateRequest("CN=Other CA", otherKey, HashAlgorithmName.SHA256, RSASignaturePadding.Pkcs1)
                .CreateSelfSigned(DateTimeOffset.UtcNow.AddDays(-1), DateTimeOffset.UtcNow.AddDays(30));
            Assert.False(Worker.Validate(new X509Certificate2Collection(other), leaf, null, SslPolicyErrors.RemoteCertificateChainErrors));

            // ORBIT_CA_CERT is picked up by the worker.
            using var w = new Worker(new WorkerOptions { Server = "https://localhost:1", CaCertFile = pem, Logger = _ => { } });
            Assert.Equal(pem, w.CaCertFile);
        }
        finally
        {
            File.Delete(pem);
        }
    }
}
