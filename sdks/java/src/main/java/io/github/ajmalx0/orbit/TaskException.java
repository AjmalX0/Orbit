package io.github.ajmalx0.orbit;

/**
 * A structured task failure. Throw it from a handler to report
 * {@code POST /v1/tasks/fail} with the given code.
 *
 * <p>{@code retryable = false} fails the step immediately, regardless of its retry policy.
 */
public class TaskException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    private final String code;
    private final String errorMessage;
    private final boolean retryable;
    private final transient Object details;

    /**
     * Creates a task failure.
     *
     * @param code      error code, e.g. {@code CARD_DECLINED}
     * @param message   human-readable message
     * @param retryable whether the runtime may retry the step according to its policy
     * @param details   optional JSON-serializable details (may be {@code null})
     */
    public TaskException(String code, String message, boolean retryable, Object details) {
        super(code + ": " + message);
        this.code = code;
        this.errorMessage = message;
        this.retryable = retryable;
        this.details = details;
    }

    /**
     * Creates a task failure without details.
     *
     * @param code      error code
     * @param message   human-readable message
     * @param retryable whether the runtime may retry the step
     */
    public TaskException(String code, String message, boolean retryable) {
        this(code, message, retryable, null);
    }

    /**
     * A failure the runtime may retry according to the step's policy.
     *
     * @param code    error code
     * @param message human-readable message
     * @return the exception, ready to throw
     */
    public static TaskException retryable(String code, String message) {
        return new TaskException(code, message, true, null);
    }

    /**
     * A failure that fails the step immediately, regardless of its retry policy.
     *
     * @param code    error code
     * @param message human-readable message
     * @return the exception, ready to throw
     */
    public static TaskException permanent(String code, String message) {
        return new TaskException(code, message, false, null);
    }

    /** @return the error code reported as {@code error_code} */
    public String code() {
        return code;
    }

    /** @return the message reported as {@code error_message}, without the code prefix */
    public String errorMessage() {
        return errorMessage;
    }

    /** @return whether the runtime may retry the step */
    public boolean retryable() {
        return retryable;
    }

    /** @return the details reported with the failure, or {@code null} */
    public Object details() {
        return details;
    }
}
