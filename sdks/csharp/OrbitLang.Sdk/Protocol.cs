using System.Collections.Generic;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace OrbitLang.Sdk;

// Wire messages of protocol/orbit/worker/v1/worker.proto (HTTP/JSON binding).

internal sealed record TaskContract(
    [property: JsonPropertyName("task_name")] string TaskName,
    [property: JsonPropertyName("contract_version")] string ContractVersion);

internal sealed record RegisterWorkerRequest(
    [property: JsonPropertyName("worker_id")] string WorkerId,
    [property: JsonPropertyName("runtime")] string Runtime,
    [property: JsonPropertyName("sdk")] string Sdk,
    [property: JsonPropertyName("contracts")] IReadOnlyList<TaskContract> Contracts);

internal sealed class RegisterWorkerResponse
{
    [JsonPropertyName("accepted")] public bool Accepted { get; set; }
    [JsonPropertyName("worker_session_id")] public string? WorkerSessionId { get; set; }
    [JsonPropertyName("heartbeat_interval_ms")] public long HeartbeatIntervalMs { get; set; }
    [JsonPropertyName("session_timeout_ms")] public long SessionTimeoutMs { get; set; }
    [JsonPropertyName("warnings")] public List<string>? Warnings { get; set; }
}

internal sealed record SessionRequest([property: JsonPropertyName("worker_session_id")] string WorkerSessionId);

internal sealed record PollTaskRequest(
    [property: JsonPropertyName("worker_session_id")] string WorkerSessionId,
    [property: JsonPropertyName("wait_ms")] long WaitMs);

internal sealed class HeartbeatResponse
{
    [JsonPropertyName("accepted")] public bool Accepted { get; set; }
    [JsonPropertyName("cancel")] public List<string>? Cancel { get; set; }
}

internal sealed class ExecuteTaskRequest
{
    [JsonPropertyName("token")] public string Token { get; set; } = "";
    [JsonPropertyName("execution_id")] public string ExecutionId { get; set; } = "";
    [JsonPropertyName("workflow")] public string Workflow { get; set; } = "";
    [JsonPropertyName("workflow_version")] public string WorkflowVersion { get; set; } = "";
    [JsonPropertyName("step_id")] public string StepId { get; set; } = "";
    [JsonPropertyName("task_name")] public string TaskName { get; set; } = "";
    [JsonPropertyName("contract_version")] public string ContractVersion { get; set; } = "";
    [JsonPropertyName("input")] public JsonElement Input { get; set; }
    [JsonPropertyName("idempotency_key")] public string IdempotencyKey { get; set; } = "";
    [JsonPropertyName("attempt")] public int Attempt { get; set; }
    [JsonPropertyName("redelivery")] public bool Redelivery { get; set; }
    [JsonPropertyName("deadline_ms")] public long? DeadlineMs { get; set; }
    [JsonPropertyName("timeout_ms")] public long? TimeoutMs { get; set; }
}

internal sealed record CompleteTaskRequest(
    [property: JsonPropertyName("token")] string Token,
    [property: JsonPropertyName("output")] JsonElement Output);

internal sealed record FailTaskRequest(
    [property: JsonPropertyName("token")] string Token,
    [property: JsonPropertyName("error_code")] string ErrorCode,
    [property: JsonPropertyName("error_message")] string ErrorMessage,
    [property: JsonPropertyName("retryable")] bool Retryable,
    [property: JsonPropertyName("details")] JsonElement? Details);

internal sealed class TaskResultAck
{
    [JsonPropertyName("accepted")] public bool Accepted { get; set; }
    [JsonPropertyName("reason")] public string? Reason { get; set; }
}
