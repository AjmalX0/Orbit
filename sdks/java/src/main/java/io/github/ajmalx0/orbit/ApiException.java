package io.github.ajmalx0.orbit;

/**
 * An error response from the ORBIT runtime, or a connection failure ({@link #status()} is 0).
 */
public final class ApiException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    private final int status;
    private final String code;

    ApiException(int status, String code, String message, Throwable cause) {
        super(code + " (" + status + "): " + message, cause);
        this.status = status;
        this.code = code;
    }

    /** @return the HTTP status, or 0 when the runtime could not be reached */
    public int status() {
        return status;
    }

    /** @return the runtime's error code, e.g. {@code UNKNOWN_SESSION}, or {@code CONNECTION} */
    public String code() {
        return code;
    }

    /** @return true when the runtime could not be reached at all */
    public boolean isConnectionError() {
        return status == 0;
    }

    /** @return true for errors worth retrying: connection failures and 5xx responses */
    boolean isTransient() {
        return status == 0 || status >= 500;
    }

    boolean isUnknownSession() {
        return status == 404 && "UNKNOWN_SESSION".equals(code);
    }
}
