using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace OrbitLang.Sdk.Tests;

/// <summary>A scripted ORBIT runtime implementing the worker protocol (mirrors sdks/go/orbit_test.go).</summary>
internal sealed class FakeRuntime : IDisposable
{
    private readonly HttpListener _listener = new();
    private readonly Task _loop;

    public readonly object Lock = new();
    public readonly List<JsonObject> Tasks = new();
    public int Registrations;
    public bool ForgetOnce;          // answer the next poll with UNKNOWN_SESSION
    public int FailComplete;         // answer this many /complete calls with 503
    public readonly Dictionary<string, JsonNode?> Completed = new();
    public readonly Dictionary<string, JsonObject> Failed = new();
    public List<string> Cancel = new();
    public string? AuthHeader;
    public JsonObject? LastRegistration;
    public int Unregistrations;

    public string Url { get; }

    public FakeRuntime()
    {
        var probe = new TcpListener(IPAddress.Loopback, 0);
        probe.Start();
        var port = ((IPEndPoint)probe.LocalEndpoint).Port;
        probe.Stop();
        Url = $"http://127.0.0.1:{port}";
        _listener.Prefixes.Add(Url + "/");
        _listener.Start();
        _loop = Task.Run(AcceptLoop);
    }

    private async Task AcceptLoop()
    {
        while (_listener.IsListening)
        {
            HttpListenerContext context;
            try
            {
                context = await _listener.GetContextAsync();
            }
            catch (Exception)
            {
                return;
            }
            _ = Task.Run(() => Handle(context));
        }
    }

    private async Task Handle(HttpListenerContext http)
    {
        string body;
        using (var reader = new StreamReader(http.Request.InputStream, Encoding.UTF8)) body = await reader.ReadToEndAsync();
        var req = string.IsNullOrEmpty(body) ? new JsonObject() : JsonNode.Parse(body)!.AsObject();
        int status;
        object? reply;
        var path = http.Request.Url!.AbsolutePath;
        if (path == "/v1/workers/poll")
        {
            bool empty;
            lock (Lock) empty = Tasks.Count == 0 && !ForgetOnce;
            if (empty) await Task.Delay(20);
        }
        lock (Lock)
        {
            AuthHeader = http.Request.Headers["authorization"];
            (status, reply) = Route(path, req);
        }
        try
        {
            http.Response.StatusCode = status;
            if (reply is not null)
            {
                var bytes = Encoding.UTF8.GetBytes(JsonSerializer.Serialize(reply));
                http.Response.ContentType = "application/json";
                http.Response.ContentLength64 = bytes.Length;
                await http.Response.OutputStream.WriteAsync(bytes);
            }
            http.Response.Close();
        }
        catch (Exception)
        {
            // client went away
        }
    }

    private (int, object?) Route(string path, JsonObject req)
    {
        static object Error(string code, string message) => new { error = new { code, message } };
        switch (path)
        {
            case "/v1/workers/register":
                Registrations++;
                LastRegistration = req;
                return (200, new { accepted = true, worker_session_id = "s", heartbeat_interval_ms = 500 });
            case "/v1/workers/poll":
                if (ForgetOnce)
                {
                    ForgetOnce = false;
                    return (404, Error("UNKNOWN_SESSION", "gone"));
                }
                if (Tasks.Count == 0) return (204, null);
                var t = Tasks[0];
                Tasks.RemoveAt(0);
                return (200, t);
            case "/v1/workers/heartbeat":
                return (200, new { accepted = true, cancel = Cancel.ToArray() });
            case "/v1/tasks/complete":
                if (FailComplete > 0)
                {
                    FailComplete--;
                    return (503, Error("UNAVAILABLE", "try later"));
                }
                Completed[(string)req["token"]!] = req["output"]?.DeepClone();
                return (200, new { accepted = true });
            case "/v1/tasks/fail":
                Failed[(string)req["token"]!] = req;
                return (200, new { accepted = true });
            case "/v1/workers/unregister":
                Unregistrations++;
                return (200, new { accepted = true });
            default:
                return (200, new { accepted = true });
        }
    }

    public static JsonObject MakeTask(string token, string name, object? input, long? deadlineMs = null)
    {
        var t = new JsonObject
        {
            ["token"] = token,
            ["execution_id"] = "e",
            ["step_id"] = token,
            ["task_name"] = name,
            ["contract_version"] = "1.0.0",
            ["input"] = JsonSerializer.SerializeToNode(input),
            ["idempotency_key"] = "e/" + token,
            ["attempt"] = 1,
        };
        if (deadlineMs is long d) t["deadline_ms"] = d;
        return t;
    }

    public void Dispose()
    {
        _listener.Stop();
        _listener.Close();
        try { _loop.Wait(TimeSpan.FromSeconds(2)); } catch (AggregateException) { }
    }
}
