package io.github.ajmalx0.orbit;

import com.fasterxml.jackson.databind.JsonNode;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.node.ObjectNode;
import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Deque;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/** A scripted ORBIT runtime implementing the worker protocol (mirrors the Go SDK's test fake). */
final class FakeRuntime implements AutoCloseable {
    static final ObjectMapper JSON = new ObjectMapper();

    final Deque<JsonNode> tasks = new ArrayDeque<>();
    int registrations;
    int unregistrations;
    boolean forgetOnce; // answer the next poll with UNKNOWN_SESSION
    int failComplete; // answer this many /complete calls with 503
    final Map<String, JsonNode> completed = new HashMap<>();
    final Map<String, JsonNode> failed = new HashMap<>();
    List<String> cancel = new ArrayList<>();
    String authHeader;
    JsonNode registerBody;

    private final HttpServer server;
    private final ExecutorService pool = Executors.newCachedThreadPool();

    FakeRuntime() throws IOException {
        this(HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0));
    }

    FakeRuntime(HttpServer server) {
        this.server = server;
        server.createContext("/", this::handle);
        server.setExecutor(pool);
        server.start();
    }

    String url() {
        String scheme = server instanceof com.sun.net.httpserver.HttpsServer ? "https" : "http";
        return scheme + "://localhost:" + server.getAddress().getPort();
    }

    static ObjectNode task(String token, String name, Object input) {
        ObjectNode t = JSON.createObjectNode()
                .put("token", token)
                .put("execution_id", "e")
                .put("step_id", token)
                .put("task_name", name)
                .put("contract_version", "1.0.0")
                .put("idempotency_key", "e/" + token)
                .put("attempt", 1);
        t.set("input", JSON.valueToTree(input));
        return t;
    }

    private void handle(HttpExchange ex) throws IOException {
        JsonNode req;
        byte[] body = ex.getRequestBody().readAllBytes();
        req = body.length == 0 ? JSON.createObjectNode() : JSON.readTree(body);
        String path = ex.getRequestURI().getPath();
        synchronized (this) {
            authHeader = ex.getRequestHeaders().getFirst("authorization");
        }
        switch (path) {
            case "/v1/workers/register" -> {
                synchronized (this) {
                    registrations++;
                    registerBody = req;
                }
                reply(ex, 200, Map.of("accepted", true, "worker_session_id", "s", "heartbeat_interval_ms", 500));
            }
            case "/v1/workers/poll" -> {
                JsonNode next;
                synchronized (this) {
                    if (forgetOnce) {
                        forgetOnce = false;
                        reply(ex, 404, Map.of("error", Map.of("code", "UNKNOWN_SESSION", "message", "gone")));
                        return;
                    }
                    next = tasks.poll();
                }
                if (next == null) {
                    sleep(20);
                    ex.sendResponseHeaders(204, -1);
                    ex.close();
                    return;
                }
                reply(ex, 200, next);
            }
            case "/v1/workers/heartbeat" -> {
                List<String> c;
                synchronized (this) {
                    c = new ArrayList<>(cancel);
                }
                reply(ex, 200, Map.of("accepted", true, "cancel", c));
            }
            case "/v1/tasks/complete" -> {
                synchronized (this) {
                    if (failComplete > 0) {
                        failComplete--;
                        reply(ex, 503, Map.of("error", Map.of("code", "UNAVAILABLE", "message", "try later")));
                        return;
                    }
                    completed.put(req.path("token").asText(), req.get("output"));
                }
                reply(ex, 200, Map.of("accepted", true));
            }
            case "/v1/tasks/fail" -> {
                synchronized (this) {
                    failed.put(req.path("token").asText(), req);
                }
                reply(ex, 200, Map.of("accepted", true));
            }
            case "/v1/workers/unregister" -> {
                synchronized (this) {
                    unregistrations++;
                }
                reply(ex, 200, Map.of("accepted", true));
            }
            default -> reply(ex, 200, Map.of("accepted", true));
        }
    }

    private static void reply(HttpExchange ex, int status, Object v) throws IOException {
        byte[] out = JSON.writeValueAsBytes(v);
        ex.getResponseHeaders().set("content-type", "application/json");
        ex.sendResponseHeaders(status, out.length);
        try (OutputStream os = ex.getResponseBody()) {
            os.write(out);
        }
    }

    private static void sleep(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    @Override
    public void close() {
        server.stop(0);
        pool.shutdownNow();
    }
}
