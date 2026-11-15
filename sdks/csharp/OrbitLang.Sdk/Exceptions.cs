using System;

namespace OrbitLang.Sdk;

/// <summary>
/// A structured task failure. Throw it from a handler to fail the attempt with
/// <paramref name="code"/>; the runtime retries it according to the step's retry
/// policy only when <see cref="Retryable"/> is <c>true</c>.
/// </summary>
/// <param name="code">Error code, e.g. <c>CARD_DECLINED</c>.</param>
/// <param name="message">Human-readable message.</param>
/// <param name="retryable"><c>false</c> fails the step immediately, regardless of its retry policy.</param>
/// <param name="details">Optional JSON-serializable details.</param>
public class TaskException(string code, string message, bool retryable = true, object? details = null) : Exception(message)
{
    /// <summary>Error code reported to the runtime.</summary>
    public string Code { get; } = code;

    /// <summary>Whether the runtime may retry the step.</summary>
    public bool Retryable { get; } = retryable;

    /// <summary>Optional JSON-serializable details reported with the failure.</summary>
    public object? Details { get; } = details;

    /// <summary>An error that fails the step immediately, regardless of its retry policy.</summary>
    public static TaskException Permanent(string code, string message, object? details = null) => new(code, message, false, details);

    /// <summary>An error that the runtime may retry according to the step's retry policy.</summary>
    public static TaskException Transient(string code, string message, object? details = null) => new(code, message, true, details);
}

/// <summary>An error response from the runtime, or a connection failure (<see cref="Status"/> 0).</summary>
public class OrbitApiException(int status, string code, string message, Exception? inner = null)
    : Exception($"{code} ({status}): {message}", inner)
{
    /// <summary>HTTP status, or 0 when the runtime could not be reached.</summary>
    public int Status { get; } = status;

    /// <summary>Error code from the runtime's error body, <c>CONNECTION</c> or <c>HTTP_ERROR</c>.</summary>
    public string Code { get; } = code;

    /// <summary>True when the runtime could not be reached at all.</summary>
    public bool IsConnectionError => Status == 0;
}
