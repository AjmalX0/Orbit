using System.Net;
using System.Net.Security;
using System.Net.Sockets;
using System.Security.Authentication;
using System.Security.Cryptography;
using System.Security.Cryptography.X509Certificates;
using System.Text;
using Xunit;

namespace OrbitLang.Sdk.Tests;

/// <summary>A throwaway CA with a server certificate (127.0.0.1) and a worker client certificate, written as PEM files.</summary>
internal sealed class TestPki : IDisposable
{
    public readonly string Dir = Directory.CreateTempSubdirectory("orbit-pki-").FullName;
    public readonly X509Certificate2 Ca;
    public readonly X509Certificate2 Server; // with private key
    public string CaPem => Path.Combine(Dir, "ca.pem");
    public string ClientCertPem => Path.Combine(Dir, "client.pem");
    public string ClientKeyPem => Path.Combine(Dir, "client.key");
    public string ClientCombinedPem => Path.Combine(Dir, "client-combined.pem");

    public TestPki(string clientName = "worker-1")
    {
        var now = DateTimeOffset.UtcNow;
        using var caKey = RSA.Create(2048);
        var caReq = new CertificateRequest("CN=Orbit Test CA", caKey, HashAlgorithmName.SHA256, RSASignaturePadding.Pkcs1);
        caReq.CertificateExtensions.Add(new X509BasicConstraintsExtension(true, false, 0, true));
        caReq.CertificateExtensions.Add(new X509KeyUsageExtension(X509KeyUsageFlags.KeyCertSign | X509KeyUsageFlags.CrlSign, true));
        caReq.CertificateExtensions.Add(new X509SubjectKeyIdentifierExtension(caReq.PublicKey, false));
        Ca = caReq.CreateSelfSigned(now.AddDays(-1), now.AddDays(30));

        Server = Issue("CN=localhost", "1.3.6.1.5.5.7.3.1", san =>
        {
            san.AddDnsName("localhost");
            san.AddIpAddress(IPAddress.Loopback);
        }, out _);

        using var client = Issue($"CN={clientName}", "1.3.6.1.5.5.7.3.2", san => san.AddDnsName(clientName), out var clientKey);
        using (clientKey)
        {
            File.WriteAllText(CaPem, Ca.ExportCertificatePem());
            File.WriteAllText(ClientCertPem, client.ExportCertificatePem());
            File.WriteAllText(ClientKeyPem, clientKey.ExportPkcs8PrivateKeyPem());
            File.WriteAllText(ClientCombinedPem, client.ExportCertificatePem() + "\n" + clientKey.ExportPkcs8PrivateKeyPem());
        }
    }

    private X509Certificate2 Issue(string subject, string eku, Action<SubjectAlternativeNameBuilder> sans, out RSA key)
    {
        key = RSA.Create(2048);
        var req = new CertificateRequest(subject, key, HashAlgorithmName.SHA256, RSASignaturePadding.Pkcs1);
        req.CertificateExtensions.Add(new X509BasicConstraintsExtension(false, false, 0, false));
        req.CertificateExtensions.Add(new X509KeyUsageExtension(X509KeyUsageFlags.DigitalSignature | X509KeyUsageFlags.KeyEncipherment, true));
        req.CertificateExtensions.Add(new X509EnhancedKeyUsageExtension(new OidCollection { new Oid(eku) }, false));
        var san = new SubjectAlternativeNameBuilder();
        sans(san);
        req.CertificateExtensions.Add(san.Build());
        var serial = RandomNumberGenerator.GetBytes(8);
        using var issued = req.Create(Ca, DateTimeOffset.UtcNow.AddDays(-1), DateTimeOffset.UtcNow.AddDays(10), serial);
        using var withKey = issued.CopyWithPrivateKey(key);
        return new X509Certificate2(withKey.Export(X509ContentType.Pkcs12));
    }

    public void Dispose()
    {
        Ca.Dispose();
        Server.Dispose();
        try { Directory.Delete(Dir, true); } catch (IOException) { }
    }
}

/// <summary>
/// A minimal HTTPS ORBIT runtime on <see cref="TcpListener"/> + <see cref="SslStream"/> that
/// requires a client certificate issued by the test CA (one request per connection).
/// </summary>
internal sealed class MutualTlsRuntime : IDisposable
{
    private readonly TcpListener _listener = new(IPAddress.Loopback, 0);
    private readonly TestPki _pki;
    private readonly X509Certificate2Collection _roots;
    private readonly CancellationTokenSource _stop = new();

    public readonly object Lock = new();
    public readonly List<string> ClientNames = new();
    public int Registrations;
    public int RejectedHandshakes;

    public string Url { get; }

    public MutualTlsRuntime(TestPki pki)
    {
        _pki = pki;
        _roots = new X509Certificate2Collection(pki.Ca);
        _listener.Start();
        Url = $"https://127.0.0.1:{((IPEndPoint)_listener.LocalEndpoint).Port}";
        _ = Task.Run(AcceptLoop);
    }

    private async Task AcceptLoop()
    {
        while (!_stop.IsCancellationRequested)
        {
            TcpClient tcp;
            try { tcp = await _listener.AcceptTcpClientAsync(_stop.Token); }
            catch (Exception) { return; }
            _ = Task.Run(() => Serve(tcp));
        }
    }

    private async Task Serve(TcpClient tcp)
    {
        using (tcp)
        await using (var tls = new SslStream(tcp.GetStream(), false))
        {
            try
            {
                await tls.AuthenticateAsServerAsync(new SslServerAuthenticationOptions
                {
                    ServerCertificate = _pki.Server,
                    ClientCertificateRequired = true,
                    RemoteCertificateValidationCallback = (_, cert, chain, errors) => Worker.Validate(_roots, cert, chain, errors),
                }, _stop.Token);
            }
            catch (Exception e) when (e is AuthenticationException or IOException or OperationCanceledException)
            {
                lock (Lock) RejectedHandshakes++;
                return;
            }
            try
            {
                var client = new X509Certificate2(tls.RemoteCertificate!);
                var name = client.GetNameInfo(X509NameType.DnsName, false);
                var (path, _) = await ReadRequest(tls);
                int status;
                string body;
                lock (Lock)
                {
                    ClientNames.Add(name);
                    (status, body) = path switch
                    {
                        "/v1/workers/register" => (200, Register()),
                        "/v1/workers/poll" => (204, ""),
                        _ => (200, """{"accepted":true}"""),
                    };
                }
                if (path == "/v1/workers/poll") await Task.Delay(20);
                var bytes = Encoding.UTF8.GetBytes(body);
                var head = $"HTTP/1.1 {status} {(status == 200 ? "OK" : "No Content")}\r\nContent-Type: application/json\r\n"
                    + $"Content-Length: {bytes.Length}\r\nConnection: close\r\n\r\n";
                await tls.WriteAsync(Encoding.ASCII.GetBytes(head));
                await tls.WriteAsync(bytes);
                await tls.FlushAsync();
            }
            catch (Exception e) when (e is IOException or InvalidOperationException)
            {
                // client went away
            }
        }
    }

    private string Register()
    {
        Registrations++;
        return """{"accepted":true,"worker_session_id":"s","heartbeat_interval_ms":500}""";
    }

    private static async Task<(string Path, string Body)> ReadRequest(Stream stream)
    {
        var buffer = new MemoryStream();
        var chunk = new byte[4096];
        int headerEnd;
        while ((headerEnd = IndexOf(buffer, "\r\n\r\n"u8)) < 0)
        {
            var n = await stream.ReadAsync(chunk);
            if (n == 0) throw new IOException("connection closed");
            buffer.Write(chunk, 0, n);
        }
        var head = Encoding.ASCII.GetString(buffer.GetBuffer(), 0, headerEnd);
        var lines = head.Split("\r\n");
        var path = lines[0].Split(' ')[1];
        var length = lines.Skip(1)
            .Select(l => l.Split(':', 2))
            .Where(p => p.Length == 2 && p[0].Trim().Equals("content-length", StringComparison.OrdinalIgnoreCase))
            .Select(p => int.Parse(p[1].Trim()))
            .FirstOrDefault();
        while (buffer.Length < headerEnd + 4 + length)
        {
            var n = await stream.ReadAsync(chunk);
            if (n == 0) throw new IOException("connection closed");
            buffer.Write(chunk, 0, n);
        }
        return (path, Encoding.UTF8.GetString(buffer.GetBuffer(), headerEnd + 4, length));
    }

    private static int IndexOf(MemoryStream buffer, ReadOnlySpan<byte> needle) =>
        buffer.GetBuffer().AsSpan(0, (int)buffer.Length).IndexOf(needle);

    public void Dispose()
    {
        _stop.Cancel();
        _listener.Stop();
    }
}

[Collection("environment")]
public sealed class TlsTests
{
    private static string? Cn(X509Certificate cert) => new X509Certificate2(cert).GetNameInfo(X509NameType.SimpleName, false);

    [Fact]
    public void ClientCertificateFromOptions()
    {
        using var pki = new TestPki();
        using var worker = new Worker(new WorkerOptions
        {
            Server = "https://127.0.0.1:1",
            CaCertFile = pki.CaPem,
            ClientCertificatePath = pki.ClientCertPem,
            ClientKeyPath = pki.ClientKeyPem,
            Logger = _ => { },
        });
        var attached = Assert.Single(worker.Handler.SslOptions.ClientCertificates!.Cast<X509Certificate>());
        var cert = Assert.IsType<X509Certificate2>(attached);
        Assert.True(cert.HasPrivateKey);
        Assert.Equal("worker-1", Cn(cert));
        Assert.Same(worker.ClientCertificate, cert);
        Assert.NotNull(worker.Handler.SslOptions.RemoteCertificateValidationCallback); // ORBIT_CA_CERT still applies
    }

    [Fact]
    public void ClientCertificateFromEnvironment()
    {
        using var pki = new TestPki("env-worker");
        string?[] saved = [Environment.GetEnvironmentVariable("ORBIT_CLIENT_CERT"), Environment.GetEnvironmentVariable("ORBIT_CLIENT_KEY")];
        try
        {
            Environment.SetEnvironmentVariable("ORBIT_CLIENT_CERT", pki.ClientCertPem);
            Environment.SetEnvironmentVariable("ORBIT_CLIENT_KEY", pki.ClientKeyPem);
            using (var w = new Worker(new WorkerOptions { Server = "https://127.0.0.1:1", Logger = _ => { } }))
            {
                var cert = Assert.IsType<X509Certificate2>(Assert.Single(w.Handler.SslOptions.ClientCertificates!.Cast<X509Certificate>()));
                Assert.True(cert.HasPrivateKey);
                Assert.Equal("env-worker", Cn(cert));
            }

            // Without ORBIT_CLIENT_KEY the key is read from the certificate file.
            Environment.SetEnvironmentVariable("ORBIT_CLIENT_CERT", pki.ClientCombinedPem);
            Environment.SetEnvironmentVariable("ORBIT_CLIENT_KEY", null);
            using (var w = new Worker(new WorkerOptions { Server = "https://127.0.0.1:1", Logger = _ => { } }))
                Assert.True(w.ClientCertificate!.HasPrivateKey);

            // Options win over the environment.
            using var other = new TestPki("option-worker");
            using (var w = new Worker(new WorkerOptions
                   {
                       Server = "https://127.0.0.1:1", ClientCertificatePath = other.ClientCombinedPem, Logger = _ => { },
                   }))
                Assert.Equal("option-worker", Cn(w.ClientCertificate!));

            // A key without a certificate is a configuration error; nothing set means no client certificate.
            Environment.SetEnvironmentVariable("ORBIT_CLIENT_CERT", null);
            Environment.SetEnvironmentVariable("ORBIT_CLIENT_KEY", pki.ClientKeyPem);
            Assert.Throws<InvalidOperationException>(() => new Worker(new WorkerOptions { Server = "https://127.0.0.1:1" }));
            Environment.SetEnvironmentVariable("ORBIT_CLIENT_KEY", null);
            using (var w = new Worker(new WorkerOptions { Server = "https://127.0.0.1:1" }))
            {
                Assert.Null(w.ClientCertificate);
                Assert.Null(w.Handler.SslOptions.ClientCertificates);
            }
        }
        finally
        {
            Environment.SetEnvironmentVariable("ORBIT_CLIENT_CERT", saved[0]);
            Environment.SetEnvironmentVariable("ORBIT_CLIENT_KEY", saved[1]);
        }
    }

    [Fact]
    public void CertificateWithoutKeyIsRejected()
    {
        using var pki = new TestPki();
        Assert.ThrowsAny<CryptographicException>(() =>
            new Worker(new WorkerOptions { Server = "https://127.0.0.1:1", ClientCertificatePath = pki.ClientCertPem }));
    }

    private static async Task RunBriefly(Worker worker, Func<bool> until)
    {
        worker.Task("t", "1.0.0", (_, _) => Task.FromResult<object?>(null));
        using var stop = new CancellationTokenSource();
        var run = worker.RunAsync(stop.Token);
        var deadline = DateTime.UtcNow.AddSeconds(10);
        while (!until() && DateTime.UtcNow < deadline) await Task.Delay(20);
        stop.Cancel();
        await run.WaitAsync(TimeSpan.FromSeconds(10));
    }

    [Fact]
    public async Task MutualTlsEndToEnd()
    {
        using var pki = new TestPki();
        using var runtime = new MutualTlsRuntime(pki);

        // With the client certificate: registers, polls and unregisters over mutual TLS.
        using (var worker = new Worker(new WorkerOptions
               {
                   Server = runtime.Url, CaCertFile = pki.CaPem, ClientCertificatePath = pki.ClientCertPem,
                   ClientKeyPath = pki.ClientKeyPem, PollWait = TimeSpan.FromMilliseconds(100), Concurrency = 1, Logger = _ => { },
               }))
        {
            await RunBriefly(worker, () => { lock (runtime.Lock) return runtime.Registrations == 1 && runtime.ClientNames.Count >= 3; });
        }
        lock (runtime.Lock)
        {
            Assert.Equal(1, runtime.Registrations);
            Assert.True(runtime.ClientNames.Count >= 3, $"only {runtime.ClientNames.Count} requests");
            Assert.All(runtime.ClientNames, n => Assert.Equal("worker-1", n));
            Assert.Equal(0, runtime.RejectedHandshakes);
        }

        // Without it: the handshake is refused and the worker never registers.
        using (var worker = new Worker(new WorkerOptions
               {
                   Server = runtime.Url, CaCertFile = pki.CaPem, PollWait = TimeSpan.FromMilliseconds(100), Logger = _ => { },
               }))
        {
            await RunBriefly(worker, () => { lock (runtime.Lock) return runtime.RejectedHandshakes >= 2; });
        }
        lock (runtime.Lock)
        {
            Assert.Equal(1, runtime.Registrations);
            Assert.True(runtime.RejectedHandshakes >= 2);
        }
    }
}
