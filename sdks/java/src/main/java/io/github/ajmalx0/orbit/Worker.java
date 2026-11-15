package io.github.ajmalx0.orbit;

import com.fasterxml.jackson.core.JsonProcessingException;
import com.fasterxml.jackson.core.type.TypeReference;
import com.fasterxml.jackson.databind.DeserializationFeature;
import com.fasterxml.jackson.databind.JavaType;
import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ArrayNode;
import com.fasterxml.jackson.databind.node.ObjectNode;
import java.io.IOException;
import java.io.PrintWriter;
import java.io.StringWriter;
import java.io.UncheckedIOException;
import java.lang.System.Logger.Level;
import java.net.InetAddress;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.security.GeneralSecurityException;
import java.time.Duration;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.ScheduledThreadPoolExecutor;
import java.util.concurrent.ThreadLocalRandom;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Function;
import javax.net.ssl.SSLContext;

/**
 * Runs task handlers for the ORBIT runtime.
 *
 * <p>A worker announces the tasks it implements, long-polls the runtime for work, runs
 * the handler and reports the result over the HTTP/JSON worker protocol.
 *
 * <pre>{@code
 * record Order(String order_id) {}
 * record Reservation(String reservation_id) {}
 *
 * Worker worker = Worker.builder().workerId("inventory-java").build();
 * worker.task("inventory.reserve", "1.0.0", Order.class,
 *         (order, ctx) -> new Reservation("res-" + order.order_id()));
 * Runtime.getRuntime().addShutdownHook(new Thread(worker::stop));
 * worker.run(); // returns after stop(): in-flight tasks finish, then the worker unregisters
 * }</pre>
 *
 * <p>Delivery is at-least-once: a task can be delivered again after a crash
 * ({@link TaskContext#redelivery()}). Use {@link TaskContext#idempotencyKey()}, which is
 * stable across retries and redeliveries, to make side effects idempotent.
 */
public final class Worker {
    /** Version of this SDK. */
    public static final String VERSION = "0.4.0";
    /** The runtime address used when neither the builder nor {@code ORBIT_SERVER} sets one. */
    public static final String DEFAULT_SERVER = "http://127.0.0.1:7233";

    private static final Duration CALL_TIMEOUT = Duration.ofSeconds(30);
    private static final long MIN_HEARTBEAT_MS = 500;
    private static final long MAX_BACKOFF_MS = 5000;

    private final String workerId;
    private final String server;
    private final String token;
    private final String runtime;
    private final int concurrency;
    private final Duration pollWait;
    private final Duration resultRetryTimeout;
    private final ObjectMapper mapper;
    private final System.Logger log;
    private final HttpClient http;
    private final String tlsHint;

    private final Map<String, Registered> tasks = new LinkedHashMap<>();
    private final Map<String, TaskContext> active = new ConcurrentHashMap<>();
    private final Object sessionLock = new Object();
    private volatile String session;
    private volatile long heartbeatEveryMs = 5000;

    private final AtomicBoolean started = new AtomicBoolean();
    private volatile boolean stopping;
    private final CountDownLatch stopSignal = new CountDownLatch(1);
    private final CountDownLatch terminated = new CountDownLatch(1);
    private final List<Poller> pollers = new ArrayList<>();

    private Worker(Builder b) {
        Function<String, String> env = b.env;
        this.server = stripSlash(firstNonEmpty(b.server, env.apply("ORBIT_SERVER"), DEFAULT_SERVER));
        this.token = firstNonEmpty(b.token, env.apply("ORBIT_WORKER_TOKEN"), env.apply("ORBIT_TOKEN"));
        this.workerId = firstNonEmpty(b.workerId, "java-" + hostname() + "-" + ProcessHandle.current().pid());
        this.runtime = firstNonEmpty(b.runtime, "java");
        this.concurrency = b.concurrency > 0 ? b.concurrency : 4;
        this.pollWait = b.pollWait != null && !b.pollWait.isNegative() && !b.pollWait.isZero() ? b.pollWait : Duration.ofSeconds(20);
        this.resultRetryTimeout = b.resultRetryTimeout != null ? b.resultRetryTimeout : Duration.ofMinutes(10);
        this.mapper = b.mapper != null ? b.mapper
                : new ObjectMapper().configure(DeserializationFeature.FAIL_ON_UNKNOWN_PROPERTIES, false);
        this.log = b.logger != null ? b.logger : System.getLogger("orbit");

        HttpClient.Builder hb = HttpClient.newBuilder()
                .version(HttpClient.Version.HTTP_1_1)
                .connectTimeout(Duration.ofSeconds(10));
        SSLContext ssl = b.sslContext;
        Path caCert = b.caCertFile != null ? b.caCertFile : path(env.apply("ORBIT_CA_CERT"));
        Path clientCert = b.clientCert != null ? b.clientCert : path(env.apply("ORBIT_CLIENT_CERT"));
        Path clientKey = b.clientCert != null ? b.clientKey : path(env.apply("ORBIT_CLIENT_KEY"));
        if ((clientCert == null) != (clientKey == null)) {
            throw new IllegalArgumentException("a client certificate needs both ORBIT_CLIENT_CERT and ORBIT_CLIENT_KEY");
        }
        if (ssl == null && (caCert != null || clientCert != null)) {
            try {
                ssl = Tls.context(caCert, clientCert, clientKey);
            } catch (IOException e) {
                throw new UncheckedIOException("cannot read TLS files (CA " + caCert + ", client certificate "
                        + clientCert + ", key " + clientKey + ")", e);
            } catch (GeneralSecurityException e) {
                throw new IllegalArgumentException("invalid TLS configuration: " + e.getMessage(), e);
            }
        }
        if (ssl != null) {
            hb.sslContext(ssl);
        }
        this.http = hb.build();
        this.tlsHint = server.startsWith("https:") && clientCert == null && b.sslContext == null
                ? " (if the runtime requires mutual TLS, set ORBIT_CLIENT_CERT and ORBIT_CLIENT_KEY)" : "";
    }

    /** @return a builder; unset options fall back to the {@code ORBIT_*} environment variables and defaults */
    public static Builder builder() {
        return new Builder();
    }

    /** @return a worker configured entirely from the environment and defaults */
    public static Worker create() {
        return builder().build();
    }

    /** @return this worker's id, as sent at registration */
    public String workerId() {
        return workerId;
    }

    /** @return the runtime address this worker talks to */
    public String server() {
        return server;
    }

    /** @return the Jackson mapper used for task input and output */
    public ObjectMapper objectMapper() {
        return mapper;
    }

    // ------------------------------------------------------------------ tasks

    /**
     * Registers a typed handler as the implementation of {@code name@version}.
     * Input that cannot be decoded into {@code inputType} fails the attempt with
     * {@code INVALID_INPUT} (not retryable).
     *
     * @param name      task name, exactly as declared in the {@code .orbit} file
     * @param version   contract version, exactly as declared
     * @param inputType class the input is decoded into
     * @param handler   the implementation
     * @param <I>       input type
     * @param <O>       output type
     * @return this worker
     */
    public <I, O> Worker task(String name, String version, Class<I> inputType, TaskHandler<I, O> handler) {
        return typed(name, version, mapper.constructType(inputType), handler);
    }

    /**
     * Registers a typed handler for a generic input type, e.g.
     * {@code new TypeReference<Map<String, List<String>>>() {}}.
     *
     * @param name      task name
     * @param version   contract version
     * @param inputType type the input is decoded into
     * @param handler   the implementation
     * @param <I>       input type
     * @param <O>       output type
     * @return this worker
     */
    public <I, O> Worker task(String name, String version, TypeReference<I> inputType, TaskHandler<I, O> handler) {
        return typed(name, version, mapper.constructType(inputType), handler);
    }

    /**
     * Registers a handler working on raw JSON as the implementation of {@code name@version}.
     *
     * @param name    task name
     * @param version contract version
     * @param handler the implementation
     * @return this worker
     */
    public Worker rawTask(String name, String version, RawTaskHandler handler) {
        Objects.requireNonNull(handler, "handler");
        return add(name, version, handler::handle);
    }

    private <I, O> Worker typed(String name, String version, JavaType type, TaskHandler<I, O> handler) {
        Objects.requireNonNull(handler, "handler");
        return add(name, version, (input, ctx) -> {
            I decoded;
            try {
                decoded = mapper.treeToValue(input, type);
            } catch (JsonProcessingException | IllegalArgumentException e) {
                throw new TaskException("INVALID_INPUT", "cannot decode input of " + ctx.taskName() + ": "
                        + e.getMessage(), false);
            }
            return handler.handle(decoded, ctx);
        });
    }

    private synchronized Worker add(String name, String version, RawTaskHandler impl) {
        Objects.requireNonNull(name, "name");
        Objects.requireNonNull(version, "version");
        if (started.get()) {
            throw new IllegalStateException("register tasks before the worker starts");
        }
        if (tasks.containsKey(name)) {
            throw new IllegalArgumentException("task " + name + " is already registered");
        }
        tasks.put(name, new Registered(version, impl));
        return this;
    }

    private record Registered(String version, RawTaskHandler handler) {}

    // ---------------------------------------------------------------- running

    /**
     * Polls for work until {@link #stop()} is called, then finishes in-flight tasks and
     * unregisters. Blocks the calling thread.
     *
     * @throws IllegalStateException if no task is registered or the worker already started
     * @throws ApiException          if registration is refused by the runtime (e.g. 401)
     */
    public void run() {
        synchronized (this) {
            if (tasks.isEmpty()) {
                throw new IllegalStateException("no tasks registered");
            }
            if (!started.compareAndSet(false, true)) {
                throw new IllegalStateException("the worker was already started");
            }
        }
        ScheduledThreadPoolExecutor timers = new ScheduledThreadPoolExecutor(1, r -> {
            Thread t = new Thread(r, "orbit-deadlines");
            t.setDaemon(true);
            return t;
        });
        timers.setRemoveOnCancelPolicy(true);
        try {
            if (!registerRetrying()) {
                return;
            }
            Thread heartbeat = new Thread(this::heartbeatLoop, "orbit-heartbeat");
            heartbeat.setDaemon(true);
            synchronized (pollers) {
                for (int i = 0; i < concurrency; i++) {
                    Poller p = new Poller(timers);
                    p.thread = new Thread(p, "orbit-poll-" + i);
                    pollers.add(p);
                }
                if (stopping) {
                    return;
                }
                heartbeat.start();
                for (Poller p : pollers) {
                    p.thread.start();
                }
            }
            for (Poller p : pollers) {
                joinUninterruptibly(p.thread);
            }
            // Heartbeats continue until in-flight tasks have finished, so the session stays alive.
            heartbeat.interrupt();
            joinUninterruptibly(heartbeat);
            String s = session;
            if (s != null) {
                try {
                    call("/v1/workers/unregister", Map.of("worker_session_id", s), Duration.ofSeconds(10));
                } catch (ApiException e) {
                    log.log(Level.DEBUG, "unregister failed: " + e.getMessage());
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }
            log.log(Level.INFO, "worker " + workerId + " stopped");
        } finally {
            timers.shutdownNow();
            terminated.countDown();
        }
    }

    /**
     * Runs the worker on a new thread named {@code orbit-worker}.
     *
     * @return the thread running {@link #run()}
     */
    public Thread start() {
        Thread t = new Thread(this::run, "orbit-worker");
        t.start();
        return t;
    }

    /**
     * Stops the worker gracefully: polling stops, in-flight tasks finish and deliver their
     * results, then the worker unregisters. Blocks until that is done. Safe to call from a
     * shutdown hook and more than once.
     */
    public void stop() {
        signalStop();
        if (started.get()) {
            boolean interrupted = false;
            while (true) {
                try {
                    terminated.await();
                    break;
                } catch (InterruptedException e) {
                    interrupted = true;
                }
            }
            if (interrupted) {
                Thread.currentThread().interrupt();
            }
        }
    }

    /**
     * Like {@link #stop()}, but waits at most {@code timeout} for in-flight tasks.
     *
     * @param timeout how long to wait
     * @return true when the worker has fully stopped
     * @throws InterruptedException if interrupted while waiting
     */
    public boolean stop(Duration timeout) throws InterruptedException {
        signalStop();
        return !started.get() || terminated.await(timeout.toMillis(), TimeUnit.MILLISECONDS);
    }

    private void signalStop() {
        stopping = true;
        stopSignal.countDown();
        synchronized (pollers) {
            for (Poller p : pollers) {
                p.interruptIfPolling();
            }
        }
    }

    /** Sleeps unless the worker stops first. @return false when the worker is stopping */
    private boolean pause(long millis) {
        try {
            return !stopSignal.await(millis, TimeUnit.MILLISECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return false;
        }
    }

    private boolean registerRetrying() {
        long delay = 200;
        while (!stopping) {
            try {
                register();
                return true;
            } catch (ApiException e) {
                if (!e.isTransient()) {
                    throw e;
                }
                log.log(Level.WARNING, e.getMessage() + "; retrying in " + delay + "ms");
            } catch (InterruptedException e) {
                return false;
            }
            if (!pause(delay)) {
                return false;
            }
            delay = nextDelay(delay);
        }
        return false;
    }

    private static long nextDelay(long delay) {
        return (long) (Math.min(delay * 2, MAX_BACKOFF_MS) * (0.8 + ThreadLocalRandom.current().nextDouble() * 0.4));
    }

    // ------------------------------------------------------------- sessions

    private String register() throws InterruptedException {
        synchronized (sessionLock) {
            ObjectNode body = mapper.createObjectNode();
            body.put("worker_id", workerId);
            body.put("runtime", runtime);
            body.put("sdk", "orbit-java/" + VERSION);
            ArrayNode contracts = body.putArray("contracts");
            synchronized (this) {
                tasks.forEach((name, t) -> contracts.addObject()
                        .put("task_name", name)
                        .put("contract_version", t.version()));
            }
            JsonNode r = call("/v1/workers/register", body, CALL_TIMEOUT).body();
            String id = r.path("worker_session_id").asText("");
            if (id.isEmpty()) {
                throw new ApiException(200, "INVALID_RESPONSE", "register returned no worker_session_id", null);
            }
            long hb = r.path("heartbeat_interval_ms").asLong(0);
            if (hb > 0) {
                heartbeatEveryMs = Math.max(hb, MIN_HEARTBEAT_MS);
            }
            for (JsonNode w : r.path("warnings")) {
                log.log(Level.WARNING, "runtime: " + w.asText());
            }
            session = id;
            log.log(Level.INFO, "worker " + workerId + " registered (" + tasks.size() + " tasks)");
            return id;
        }
    }

    /** Re-registers unless another thread already replaced the stale session. */
    private String reRegister(String stale) throws InterruptedException {
        synchronized (sessionLock) {
            String current = session;
            if (current != null && !current.equals(stale)) {
                return current;
            }
            return register();
        }
    }

    private interface SessionCall {
        Response call(String session) throws InterruptedException;
    }

    /**
     * Calls {@code fn} with the current session, re-registering once if the runtime no
     * longer knows it (for example after a restart).
     */
    private Response withSession(SessionCall fn) throws InterruptedException {
        String s = session;
        if (s == null) {
            s = reRegister(null);
        }
        try {
            return fn.call(s);
        } catch (ApiException e) {
            if (!e.isUnknownSession()) {
                throw e;
            }
            log.log(Level.INFO, "runtime does not know session; registering again");
            return fn.call(reRegister(s));
        }
    }

    // ---------------------------------------------------------------- loops

    private void heartbeatLoop() {
        while (true) {
            try {
                Thread.sleep(heartbeatEveryMs);
                Response r = withSession(s -> call("/v1/workers/heartbeat", Map.of("worker_session_id", s), CALL_TIMEOUT));
                for (JsonNode t : r.body().path("cancel")) {
                    TaskContext ctx = active.get(t.asText());
                    if (ctx != null && !ctx.isCancelled()) {
                        log.log(Level.INFO, "cancellation requested for " + shortToken(ctx.token()));
                        ctx.cancel();
                    }
                }
            } catch (InterruptedException e) {
                return;
            } catch (ApiException e) {
                log.log(Level.DEBUG, "heartbeat failed: " + e.getMessage());
            } catch (RuntimeException e) {
                log.log(Level.WARNING, "heartbeat failed", e);
            }
        }
    }

    /** One long-poll loop. Only interrupted by {@link #stop()} while it waits for a task. */
    private final class Poller implements Runnable {
        private final ScheduledThreadPoolExecutor timers;
        private Thread thread;
        private boolean polling;

        Poller(ScheduledThreadPoolExecutor timers) {
            this.timers = timers;
        }

        synchronized void interruptIfPolling() {
            if (polling) {
                thread.interrupt();
            }
        }

        private synchronized boolean beginPoll() {
            if (stopping) {
                return false;
            }
            polling = true;
            return true;
        }

        private synchronized void endPoll() {
            polling = false;
            Thread.interrupted();
        }

        @Override
        public void run() {
            long backoff = 200;
            ObjectNode pollBody = mapper.createObjectNode().put("wait_ms", pollWait.toMillis());
            Duration timeout = pollWait.plusSeconds(15);
            while (beginPoll()) {
                Response r = null;
                ApiException failure = null;
                try {
                    r = withSession(s -> call("/v1/workers/poll", pollBody.deepCopy().put("worker_session_id", s), timeout));
                } catch (InterruptedException e) {
                    // stop() interrupted the long poll
                } catch (ApiException e) {
                    failure = e;
                } catch (RuntimeException e) {
                    failure = new ApiException(0, "CLIENT_ERROR", e.toString(), e);
                } finally {
                    endPoll();
                }
                if (stopping && r == null) {
                    return;
                }
                if (failure != null) {
                    log.log(Level.WARNING, "poll failed: " + failure.getMessage() + "; retrying in " + backoff + "ms");
                    if (!pause(backoff)) {
                        return;
                    }
                    backoff = Math.min(backoff * 2, MAX_BACKOFF_MS);
                    continue;
                }
                backoff = 200;
                if (r == null || r.status() == 204 || r.body().path("token").asText("").isEmpty()) {
                    continue;
                }
                execute(r.body(), timers);
            }
        }
    }

    // -------------------------------------------------------------- execute

    private void execute(JsonNode task, ScheduledThreadPoolExecutor timers) {
        TaskContext ctx = new TaskContext(task);
        active.put(ctx.token(), ctx);
        ScheduledFuture<?> deadline = null;
        if (ctx.deadlineMs().isPresent()) {
            long in = ctx.deadlineMs().getAsLong() - System.currentTimeMillis();
            deadline = timers.schedule(ctx::cancel, Math.max(0, in), TimeUnit.MILLISECONDS);
        }
        try {
            ObjectNode output = invoke(ctx);
            ObjectNode body = mapper.createObjectNode().put("token", ctx.token());
            body.set("output", output);
            deliver("/v1/tasks/complete", body);
        } catch (TaskException e) {
            log.log(Level.INFO, "task " + ctx.taskName() + " failed: " + e.getMessage());
            fail(ctx, e.code(), e.errorMessage(), e.retryable(), e.details());
        } catch (Throwable e) {
            if (e instanceof InterruptedException && ctx.isCancelled()) {
                fail(ctx, "CANCELLED", "the attempt was cancelled", true, null);
                return;
            }
            String trace = stackTrace(e);
            log.log(Level.ERROR, "task " + ctx.taskName() + " raised " + trace);
            Map<String, Object> details = new LinkedHashMap<>();
            details.put("exception", e.getClass().getName());
            details.put("stack_trace", trace);
            fail(ctx, "UNHANDLED_EXCEPTION", e.toString(), true, details);
        } finally {
            if (deadline != null) {
                deadline.cancel(false);
            }
            active.remove(ctx.token());
        }
    }

    private ObjectNode invoke(TaskContext ctx) throws Exception {
        Registered impl;
        synchronized (this) {
            impl = tasks.get(ctx.taskName());
        }
        if (impl == null) {
            throw TaskException.retryable("UNKNOWN_TASK", "worker does not implement " + ctx.taskName());
        }
        log.log(Level.DEBUG, "executing " + ctx.taskName() + " step=" + ctx.stepId() + " attempt=" + ctx.attempt()
                + (ctx.redelivery() ? " (redelivery)" : ""));
        Object result;
        ctx.begin();
        try {
            result = impl.handler().handle(ctx.input(), ctx);
        } finally {
            ctx.end();
        }
        if (result == null) {
            return mapper.createObjectNode();
        }
        JsonNode node;
        try {
            node = mapper.valueToTree(result);
        } catch (IllegalArgumentException e) {
            throw new TaskException("INVALID_RESULT", "cannot serialize the result of " + ctx.taskName() + ": "
                    + e.getMessage(), false);
        }
        if (node == null || node.isNull()) {
            return mapper.createObjectNode();
        }
        if (!node.isObject()) {
            throw new TaskException("INVALID_RESULT", "handler returned " + result.getClass().getSimpleName()
                    + " (JSON " + node.getNodeType().name().toLowerCase(java.util.Locale.ROOT)
                    + "); tasks must return a JSON object", false);
        }
        return (ObjectNode) node;
    }

    private void fail(TaskContext ctx, String code, String message, boolean retryable, Object details) {
        ObjectNode body = mapper.createObjectNode()
                .put("token", ctx.token())
                .put("error_code", code)
                .put("error_message", message == null ? "" : message)
                .put("retryable", retryable);
        if (details != null) {
            try {
                body.set("details", mapper.valueToTree(details));
            } catch (IllegalArgumentException e) {
                body.put("details", String.valueOf(details));
            }
        }
        deliver("/v1/tasks/fail", body);
    }

    /**
     * Reports a result, retrying through connection errors and 5xx responses so that the
     * result survives a runtime restart (the runtime reconciles it). {@code accepted:false}
     * is final.
     */
    private void deliver(String path, ObjectNode body) {
        long giveUpAt = System.nanoTime() + resultRetryTimeout.toNanos();
        long delay = 200;
        boolean interrupted = false;
        try {
            while (true) {
                try {
                    JsonNode ack = call(path, body, CALL_TIMEOUT).body();
                    if (!ack.path("accepted").asBoolean(true)) {
                        log.log(Level.INFO, "runtime ignored result for " + shortToken(body.path("token").asText())
                                + ": " + ack.path("reason").asText(""));
                    }
                    return;
                } catch (ApiException e) {
                    if (!e.isTransient()) {
                        log.log(Level.WARNING, "could not deliver result: " + e.getMessage());
                        return;
                    }
                    if (System.nanoTime() - giveUpAt > 0) {
                        log.log(Level.ERROR, "could not deliver result: " + e.getMessage() + "; giving up");
                        return;
                    }
                    log.log(Level.WARNING, e.getMessage() + "; retrying in " + delay + "ms");
                } catch (InterruptedException e) {
                    interrupted = true;
                }
                try {
                    Thread.sleep(delay);
                } catch (InterruptedException e) {
                    interrupted = true;
                }
                delay = nextDelay(delay);
            }
        } finally {
            if (interrupted) {
                Thread.currentThread().interrupt();
            }
        }
    }

    // ------------------------------------------------------------------ HTTP

    private record Response(int status, JsonNode body) {}

    private Response call(String path, Object body, Duration timeout) throws InterruptedException {
        byte[] payload;
        try {
            payload = mapper.writeValueAsBytes(body);
        } catch (JsonProcessingException e) {
            throw new IllegalArgumentException("cannot encode request: " + e.getMessage(), e);
        }
        HttpRequest.Builder req = HttpRequest.newBuilder(URI.create(server + path))
                .timeout(timeout)
                .header("content-type", "application/json")
                .POST(HttpRequest.BodyPublishers.ofByteArray(payload));
        if (token != null) {
            req.header("authorization", "Bearer " + token);
        }
        HttpResponse<byte[]> resp;
        try {
            resp = http.send(req.build(), HttpResponse.BodyHandlers.ofByteArray());
        } catch (IOException e) {
            throw new ApiException(0, "CONNECTION", "cannot reach ORBIT runtime at " + server + ": " + e + tlsHint, e);
        }
        byte[] raw = resp.body() == null ? new byte[0] : resp.body();
        int status = resp.statusCode();
        if (status >= 400) {
            String code = "HTTP_ERROR";
            String message = new String(raw, StandardCharsets.UTF_8);
            try {
                JsonNode err = mapper.readTree(raw).path("error");
                if (err.hasNonNull("code")) {
                    code = err.get("code").asText();
                    message = err.path("message").asText("");
                }
            } catch (IOException ignored) {
                // not JSON: keep the raw body as the message
            }
            throw new ApiException(status, code, message, null);
        }
        JsonNode parsed = mapper.createObjectNode();
        if (raw.length > 0) {
            try {
                parsed = mapper.readTree(raw);
            } catch (IOException e) {
                throw new ApiException(status, "INVALID_RESPONSE", "runtime sent invalid JSON: " + e.getMessage(), e);
            }
        }
        return new Response(status, parsed);
    }

    // --------------------------------------------------------------- helpers

    private static String firstNonEmpty(String... values) {
        for (String v : values) {
            if (v != null && !v.isEmpty()) {
                return v;
            }
        }
        return null;
    }

    private static Path path(String s) {
        return s == null || s.isEmpty() ? null : Path.of(s);
    }

    private static String stripSlash(String s) {
        while (s.endsWith("/")) {
            s = s.substring(0, s.length() - 1);
        }
        return s;
    }

    private static String hostname() {
        String h = System.getenv("HOSTNAME");
        if (h != null && !h.isEmpty()) {
            return h;
        }
        try {
            return InetAddress.getLocalHost().getHostName();
        } catch (IOException e) {
            return "localhost";
        }
    }

    private static String shortToken(String token) {
        return token.substring(0, Math.min(8, token.length()));
    }

    private static String stackTrace(Throwable e) {
        StringWriter w = new StringWriter();
        e.printStackTrace(new PrintWriter(w));
        return w.toString();
    }

    private static void joinUninterruptibly(Thread t) {
        boolean interrupted = false;
        while (true) {
            try {
                t.join();
                break;
            } catch (InterruptedException e) {
                interrupted = true;
            }
        }
        if (interrupted) {
            Thread.currentThread().interrupt();
        }
    }

    // --------------------------------------------------------------- builder

    /**
     * Configures a {@link Worker}. Unset options use the {@code ORBIT_SERVER},
     * {@code ORBIT_WORKER_TOKEN} / {@code ORBIT_TOKEN}, {@code ORBIT_CA_CERT} and
     * {@code ORBIT_CLIENT_CERT} / {@code ORBIT_CLIENT_KEY}
     * environment variables, then defaults.
     */
    public static final class Builder {
        private String workerId;
        private String server;
        private String token;
        private Path caCertFile;
        private Path clientCert;
        private Path clientKey;
        private SSLContext sslContext;
        private int concurrency;
        private String runtime;
        private Duration pollWait;
        private Duration resultRetryTimeout;
        private ObjectMapper mapper;
        private System.Logger logger;
        private Function<String, String> env = System::getenv;

        private Builder() {}

        /**
         * @param workerId stable identity (default {@code java-<host>-<pid>})
         * @return this builder
         */
        public Builder workerId(String workerId) {
            this.workerId = workerId;
            return this;
        }

        /**
         * @param server runtime address (default {@code ORBIT_SERVER}, then {@value Worker#DEFAULT_SERVER})
         * @return this builder
         */
        public Builder server(String server) {
            this.server = server;
            return this;
        }

        /**
         * @param token worker credential sent as {@code Authorization: Bearer}
         *              (default {@code ORBIT_WORKER_TOKEN}, then {@code ORBIT_TOKEN})
         * @return this builder
         */
        public Builder token(String token) {
            this.token = token;
            return this;
        }

        /**
         * @param caCertFile PEM CA certificate(s) to trust for HTTPS, in addition to the
         *                   system trust store (default {@code ORBIT_CA_CERT})
         * @return this builder
         */
        public Builder caCertFile(Path caCertFile) {
            this.caCertFile = caCertFile;
            return this;
        }

        /**
         * Presents a client certificate for mutual TLS; the runtime identifies the worker by
         * the certificate's DNS SAN or subject CN (default {@code ORBIT_CLIENT_CERT} and
         * {@code ORBIT_CLIENT_KEY}).
         *
         * @param certPem PEM certificate chain, leaf first
         * @param keyPem  PEM private key in PKCS#8 form ({@code BEGIN PRIVATE KEY}), RSA or EC
         * @return this builder
         */
        public Builder clientCert(Path certPem, Path keyPem) {
            this.clientCert = Objects.requireNonNull(certPem, "certPem");
            this.clientKey = Objects.requireNonNull(keyPem, "keyPem");
            return this;
        }

        /**
         * @param sslContext TLS context to use instead of the CA and client certificate options
         * @return this builder
         */
        public Builder sslContext(SSLContext sslContext) {
            this.sslContext = sslContext;
            return this;
        }

        /**
         * @param concurrency number of concurrent poll loops, i.e. tasks run at once (default 4)
         * @return this builder
         */
        public Builder concurrency(int concurrency) {
            this.concurrency = concurrency;
            return this;
        }

        /**
         * @param runtime language announced at registration, matched against {@code using <language>}
         *                (default {@code java})
         * @return this builder
         */
        public Builder runtime(String runtime) {
            this.runtime = runtime;
            return this;
        }

        /**
         * @param pollWait how long one long poll waits for a task (default 20s, runtime max 60s)
         * @return this builder
         */
        public Builder pollWait(Duration pollWait) {
            this.pollWait = pollWait;
            return this;
        }

        /**
         * @param timeout how long to keep retrying a result through outages (default 10 minutes)
         * @return this builder
         */
        public Builder resultRetryTimeout(Duration timeout) {
            this.resultRetryTimeout = timeout;
            return this;
        }

        /**
         * @param mapper Jackson mapper for task input and output (default: a plain
         *               {@code ObjectMapper} that ignores unknown properties)
         * @return this builder
         */
        public Builder objectMapper(ObjectMapper mapper) {
            this.mapper = mapper;
            return this;
        }

        /**
         * @param logger where the worker logs (default {@code System.getLogger("orbit")})
         * @return this builder
         */
        public Builder logger(System.Logger logger) {
            this.logger = logger;
            return this;
        }

        /** Replaces the environment lookup (for tests). */
        Builder environment(Function<String, String> env) {
            this.env = env;
            return this;
        }

        /**
         * @return the configured worker
         * @throws UncheckedIOException     if the CA certificate cannot be read
         * @throws IllegalArgumentException if the CA certificate is invalid
         */
        public Worker build() {
            return new Worker(this);
        }
    }
}
