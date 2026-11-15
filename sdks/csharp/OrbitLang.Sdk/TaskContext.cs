using System;
using System.Text.Json;
using System.Threading;

namespace OrbitLang.Sdk;

/// <summary>Information about the attempt being executed (protocol <c>ExecuteTaskRequest</c>).</summary>
public sealed class TaskContext
{
    private readonly Action<string> _log;
    private volatile string? _cancelReason;

    internal TaskContext(ExecuteTaskRequest task, JsonElement input, CancellationToken cancellationToken, Action<string> log)
    {
        Token = task.Token;
        ExecutionId = task.ExecutionId;
        Workflow = task.Workflow;
        WorkflowVersion = task.WorkflowVersion;
        StepId = task.StepId;
        TaskName = task.TaskName;
        ContractVersion = task.ContractVersion;
        Input = input;
        IdempotencyKey = task.IdempotencyKey;
        Attempt = task.Attempt;
        Redelivery = task.Redelivery;
        DeadlineMs = task.DeadlineMs;
        TimeoutMs = task.TimeoutMs;
        CancellationToken = cancellationToken;
        _log = log;
    }

    /// <summary>Identifies this delivery.</summary>
    public string Token { get; }

    /// <summary>The workflow execution this attempt belongs to.</summary>
    public string ExecutionId { get; }

    /// <summary>Workflow name.</summary>
    public string Workflow { get; }

    /// <summary>Workflow version.</summary>
    public string WorkflowVersion { get; }

    /// <summary>Step id, <c>&lt;step&gt;</c> or <c>compensate:&lt;step&gt;</c>.</summary>
    public string StepId { get; }

    /// <summary>Task name, e.g. <c>payments.charge</c>.</summary>
    public string TaskName { get; }

    /// <summary>Contract version of the task.</summary>
    public string ContractVersion { get; }

    /// <summary>The raw task input.</summary>
    public JsonElement Input { get; }

    /// <summary>Stable across retries and redeliveries: use it to make side effects idempotent.</summary>
    public string IdempotencyKey { get; }

    /// <summary>1-based attempt number. Redeliveries keep the number.</summary>
    public int Attempt { get; }

    /// <summary>True when the outcome of a previous delivery of this attempt is unknown.</summary>
    public bool Redelivery { get; }

    /// <summary>Absolute deadline of this attempt in Unix epoch milliseconds, if any.</summary>
    public long? DeadlineMs { get; }

    /// <summary>The step timeout in milliseconds, if any.</summary>
    public long? TimeoutMs { get; }

    /// <summary>
    /// Cancelled when the runtime asks this attempt to stop (timeout or workflow
    /// cancellation), when the attempt deadline passes, or when the worker shuts down.
    /// </summary>
    public CancellationToken CancellationToken { get; }

    /// <summary>Time left before <see cref="DeadlineMs"/>, or <c>null</c> without a deadline.</summary>
    public TimeSpan? Remaining =>
        DeadlineMs is long d ? TimeSpan.FromMilliseconds(Math.Max(0, d - DateTimeOffset.UtcNow.ToUnixTimeMilliseconds())) : null;

    /// <summary>Writes a log line tagged with the execution, step and attempt.</summary>
    public void Log(string message) => _log($"[{ExecutionId}/{StepId}#{Attempt}] {message}");

    internal string? CancelReason => _cancelReason;

    internal void MarkCancelled(string reason) => _cancelReason ??= reason;
}
