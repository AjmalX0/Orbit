package io.github.ajmalx0.orbit;

/**
 * A typed task implementation. The task input is decoded into {@code I} with the
 * worker's Jackson {@code ObjectMapper}; the returned value must serialize to a JSON object.
 *
 * @param <I> input type
 * @param <O> output type
 */
@FunctionalInterface
public interface TaskHandler<I, O> {
    /**
     * Runs one attempt of the task.
     *
     * @param input the decoded task input
     * @param ctx   information about the attempt, including cancellation
     * @return the task output; {@code null} is reported as {@code {}}
     * @throws Exception any failure; {@link TaskException} is reported as given, anything
     *                   else as a retryable {@code UNHANDLED_EXCEPTION}
     */
    O handle(I input, TaskContext ctx) throws Exception;
}
