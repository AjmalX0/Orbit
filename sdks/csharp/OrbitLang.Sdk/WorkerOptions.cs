using System;
using System.Text.Json;

namespace OrbitLang.Sdk;

/// <summary>
/// Configures a <see cref="Worker"/>. Unset values use sensible defaults and the
/// <c>ORBIT_SERVER</c>, <c>ORBIT_WORKER_TOKEN</c> / <c>ORBIT_TOKEN</c> and
/// <c>ORBIT_CA_CERT</c> environment variables.
/// </summary>
public sealed class WorkerOptions
{
    /// <summary>Stable worker identity. Defaults to <c>csharp-&lt;host&gt;-&lt;pid&gt;</c>.</summary>
    public string? WorkerId { get; set; }

    /// <summary>Runtime address. Defaults to <c>ORBIT_SERVER</c>, then <c>http://127.0.0.1:7233</c>.</summary>
    public string? Server { get; set; }

    /// <summary>Bearer token. Defaults to <c>ORBIT_WORKER_TOKEN</c>, then <c>ORBIT_TOKEN</c>.</summary>
    public string? Token { get; set; }

    /// <summary>Path of a PEM CA certificate to trust for HTTPS. Defaults to <c>ORBIT_CA_CERT</c>.</summary>
    public string? CaCertFile { get; set; }

    /// <summary>Number of concurrent long-poll loops, i.e. the maximum number of tasks run at once. Default 4.</summary>
    public int Concurrency { get; set; } = 4;

    /// <summary>Announced runtime, matched against <c>using &lt;language&gt;</c> in workflows. Default <c>csharp</c>.</summary>
    public string Runtime { get; set; } = "csharp";

    /// <summary>How long each poll waits for a task (<c>wait_ms</c>). Default 20 seconds.</summary>
    public TimeSpan PollWait { get; set; } = TimeSpan.FromSeconds(20);

    /// <summary>
    /// Options used to deserialize task input and serialize task output. Defaults to
    /// snake_case property names (<c>OrderId</c> ↔ <c>order_id</c>), case-insensitive reads.
    /// </summary>
    public JsonSerializerOptions? JsonOptions { get; set; }

    /// <summary>Receives log lines. Defaults to standard error with an <c>[orbit]</c> prefix.</summary>
    public Action<string>? Logger { get; set; }
}
