package io.github.ajmalx0.orbit;

import com.fasterxml.jackson.databind.JsonNode;

/**
 * A task implementation working on raw JSON. The input is the task's JSON value
 * ({@code {}} when the runtime sent none).
 */
@FunctionalInterface
public interface RawTaskHandler {
    /**
     * Runs one attempt of the task.
     *
     * @param input the raw task input
     * @param ctx   information about the attempt, including cancellation
     * @return the task output: a {@code JsonNode} object, a {@code Map}, or any value
     *         that serializes to a JSON object; {@code null} is reported as {@code {}}
     * @throws Exception any failure; {@link TaskException} is reported as given, anything
     *                   else as a retryable {@code UNHANDLED_EXCEPTION}
     */
    Object handle(JsonNode input, TaskContext ctx) throws Exception;
}
