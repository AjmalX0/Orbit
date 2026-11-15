package io.github.ajmalx0.orbit;

import com.fasterxml.jackson.databind.JsonNode;
import java.util.OptionalLong;
import java.util.concurrent.CompletableFuture;

/**
 * One delivery of a task to this worker (protocol {@code ExecuteTaskRequest}), plus
 * cooperative cancellation.
 *
 * <p>The attempt is cancelled when the runtime lists its token in a heartbeat's
 * {@code cancel} list (timeout or workflow cancellation) or when its deadline passes.
 * Cancellation sets {@link #isCancelled()}, completes {@link #cancellation()} and
 * interrupts the thread running the handler, so blocking calls such as
 * {@code Thread.sleep} throw {@link InterruptedException}. Any result sent after
 * cancellation is ignored by the runtime.
 *
 * <p>Delivery is at-least-once: use {@link #idempotencyKey()}, which is stable across
 * retries and redeliveries, to make side effects idempotent.
 */
public final class TaskContext {
    private final String token;
    private final String executionId;
    private final String workflow;
    private final String workflowVersion;
    private final String stepId;
    private final String taskName;
    private final String contractVersion;
    private final JsonNode input;
    private final String idempotencyKey;
    private final int attempt;
    private final boolean redelivery;
    private final Long deadlineMs;
    private final Long timeoutMs;

    private final CompletableFuture<Void> cancelled = new CompletableFuture<>();
    private final Object lock = new Object();
    private Thread runner;

    TaskContext(JsonNode task) {
        this.token = text(task, "token");
        this.executionId = text(task, "execution_id");
        this.workflow = text(task, "workflow");
        this.workflowVersion = text(task, "workflow_version");
        this.stepId = text(task, "step_id");
        this.taskName = text(task, "task_name");
        this.contractVersion = text(task, "contract_version");
        JsonNode in = task.get("input");
        this.input = in == null || in.isNull() ? com.fasterxml.jackson.databind.node.JsonNodeFactory.instance.objectNode() : in;
        this.idempotencyKey = text(task, "idempotency_key");
        this.attempt = task.path("attempt").asInt(1);
        this.redelivery = task.path("redelivery").asBoolean(false);
        this.deadlineMs = number(task, "deadline_ms");
        this.timeoutMs = number(task, "timeout_ms");
    }

    private static String text(JsonNode n, String field) {
        JsonNode v = n.get(field);
        return v == null || v.isNull() ? "" : v.asText();
    }

    private static Long number(JsonNode n, String field) {
        JsonNode v = n.get(field);
        return v == null || !v.canConvertToLong() ? null : v.asLong();
    }

    /** @return the delivery token, echoed in the result */
    public String token() {
        return token;
    }

    /** @return the workflow execution id */
    public String executionId() {
        return executionId;
    }

    /** @return the workflow name */
    public String workflow() {
        return workflow;
    }

    /** @return the workflow version */
    public String workflowVersion() {
        return workflowVersion;
    }

    /** @return the step id, {@code "<step>"} or {@code "compensate:<step>"} */
    public String stepId() {
        return stepId;
    }

    /** @return the task name, e.g. {@code payments.charge} */
    public String taskName() {
        return taskName;
    }

    /** @return the contract version of the task */
    public String contractVersion() {
        return contractVersion;
    }

    /** @return the raw task input ({@code {}} when the runtime sent none) */
    public JsonNode input() {
        return input;
    }

    /** @return a key stable across retries and redeliveries, for idempotent side effects */
    public String idempotencyKey() {
        return idempotencyKey;
    }

    /** @return the 1-based attempt number; redeliveries keep the number */
    public int attempt() {
        return attempt;
    }

    /** @return true when the outcome of a previous delivery of this attempt is unknown */
    public boolean redelivery() {
        return redelivery;
    }

    /** @return the absolute deadline of this attempt in epoch milliseconds, if any */
    public OptionalLong deadlineMs() {
        return deadlineMs == null ? OptionalLong.empty() : OptionalLong.of(deadlineMs);
    }

    /** @return the attempt timeout in milliseconds, if any */
    public OptionalLong timeoutMs() {
        return timeoutMs == null ? OptionalLong.empty() : OptionalLong.of(timeoutMs);
    }

    /** @return milliseconds left until the deadline (never negative), if there is one */
    public OptionalLong remainingMs() {
        if (deadlineMs == null) {
            return OptionalLong.empty();
        }
        return OptionalLong.of(Math.max(0, deadlineMs - System.currentTimeMillis()));
    }

    /** @return true once the runtime asked this attempt to stop, or its deadline passed */
    public boolean isCancelled() {
        return cancelled.isDone();
    }

    /**
     * A future completed when the attempt is cancelled. Use it to wait for cancellation
     * or to register a callback, e.g. {@code ctx.cancellation().thenRun(conn::abort)}.
     *
     * @return a future that never completes exceptionally
     */
    public CompletableFuture<Void> cancellation() {
        return cancelled.copy();
    }

    /**
     * Throws if the attempt was cancelled. Call it between units of work.
     *
     * @throws TaskException with code {@code CANCELLED}
     */
    public void throwIfCancelled() {
        if (isCancelled()) {
            throw new TaskException("CANCELLED", "the attempt was cancelled", true);
        }
    }

    void cancel() {
        synchronized (lock) {
            if (cancelled.complete(null) && runner != null) {
                runner.interrupt();
            }
        }
    }

    /** Marks the calling thread as running the handler, so cancellation interrupts it. */
    void begin() {
        synchronized (lock) {
            runner = Thread.currentThread();
            if (cancelled.isDone()) {
                runner.interrupt();
            }
        }
    }

    /** Stops cancellation from interrupting the thread and clears a pending interrupt. */
    void end() {
        synchronized (lock) {
            runner = null;
        }
        Thread.interrupted();
    }
}
