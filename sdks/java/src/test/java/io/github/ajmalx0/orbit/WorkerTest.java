package io.github.ajmalx0.orbit;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

import com.fasterxml.jackson.databind.JsonNode;
import com.sun.net.httpserver.HttpsConfigurator;
import com.sun.net.httpserver.HttpsServer;
import java.io.FileInputStream;
import java.io.InputStream;
import java.net.InetSocketAddress;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyStore;
import java.time.Duration;
import java.util.List;
import java.util.Map;
import java.util.ResourceBundle;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.BooleanSupplier;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;

class WorkerTest {
    record EchoIn(int n) {}

    record EchoOut(int n) {}

    /** Keeps test output quiet; the worker logs through this. */
    static final System.Logger QUIET = new System.Logger() {
        @Override
        public String getName() {
            return "quiet";
        }

        @Override
        public boolean isLoggable(Level level) {
            return false;
        }

        @Override
        public void log(Level level, ResourceBundle bundle, String msg, Throwable thrown) {}

        @Override
        public void log(Level level, ResourceBundle bundle, String format, Object... params) {}
    };

    static void waitFor(BooleanSupplier cond) throws InterruptedException {
        long deadline = System.nanoTime() + Duration.ofSeconds(10).toNanos();
        while (!cond.getAsBoolean()) {
            if (System.nanoTime() > deadline) {
                fail("condition not reached");
            }
            Thread.sleep(10);
        }
    }

    @Test
    @Timeout(30)
    void workerProtocol() throws Exception {
        try (FakeRuntime f = new FakeRuntime()) {
            AtomicBoolean futureSawCancel = new AtomicBoolean();
            synchronized (f) {
                f.forgetOnce = true;
                f.failComplete = 2;
                f.tasks.add(FakeRuntime.task("ok", "echo", Map.of("n", 1)));
                f.tasks.add(FakeRuntime.task("biz", "fail", null));
                f.tasks.add(FakeRuntime.task("boom", "throw", null));
                f.tasks.add(FakeRuntime.task("bad", "list", null));
                f.tasks.add(FakeRuntime.task("slow", "slow", null));
            }

            Worker w = Worker.builder()
                    .server(f.url())
                    .token("wt")
                    .workerId("java-test")
                    .concurrency(2)
                    .pollWait(Duration.ofMillis(100))
                    .logger(QUIET)
                    .build();
            w.task("echo", "1.0.0", EchoIn.class, (in, ctx) -> new EchoOut(in.n() + 1));
            w.task("fail", "1.0.0", EchoIn.class, (in, ctx) -> {
                throw TaskException.permanent("CARD_DECLINED", "no funds");
            });
            w.rawTask("throw", "1.0.0", (in, ctx) -> {
                throw new IllegalStateException("kaboom");
            });
            w.rawTask("list", "1.0.0", (in, ctx) -> List.of(1, 2, 3));
            w.rawTask("slow", "1.0.0", (in, ctx) -> {
                ctx.cancellation().thenRun(() -> futureSawCancel.set(true));
                synchronized (f) {
                    f.cancel = List.of(ctx.token());
                }
                try {
                    Thread.sleep(20_000); // interrupted by the heartbeat's cancel list
                } catch (InterruptedException e) {
                    if (ctx.isCancelled()) {
                        throw TaskException.permanent("CANCELLED", "stopped");
                    }
                }
                return Map.of("unexpected", true);
            });

            Thread runner = w.start();
            waitFor(() -> {
                synchronized (f) {
                    return f.completed.size() == 1 && f.failed.size() == 4;
                }
            });
            w.stop();
            runner.join(5000);
            assertFalse(runner.isAlive(), "run() should have returned after stop()");

            synchronized (f) {
                assertEquals("{\"n\":2}", f.completed.get("ok").toString(), "output");
                assertEquals(0, f.failComplete, "complete retried through two 503s");
                assertTrue(f.registrations >= 2,
                        "expected re-registration after UNKNOWN_SESSION, got " + f.registrations);

                JsonNode biz = f.failed.get("biz");
                assertEquals("CARD_DECLINED", biz.path("error_code").asText(), biz.toString());
                assertEquals("no funds", biz.path("error_message").asText());
                assertFalse(biz.path("retryable").asBoolean(true), biz.toString());

                JsonNode boom = f.failed.get("boom");
                assertEquals("UNHANDLED_EXCEPTION", boom.path("error_code").asText(), boom.toString());
                assertTrue(boom.path("retryable").asBoolean(false), boom.toString());
                assertTrue(boom.path("error_message").asText().contains("kaboom"));
                assertTrue(boom.path("details").path("stack_trace").asText().contains("WorkerTest"),
                        "stack trace in details: " + boom);

                JsonNode bad = f.failed.get("bad");
                assertEquals("INVALID_RESULT", bad.path("error_code").asText(), bad.toString());
                assertFalse(bad.path("retryable").asBoolean(true), bad.toString());

                JsonNode slow = f.failed.get("slow");
                assertEquals("CANCELLED", slow.path("error_code").asText(), "cancellation not delivered: " + slow);
                assertTrue(futureSawCancel.get(), "cancellation future completed");

                assertEquals("Bearer wt", f.authHeader, "auth header");
                assertEquals("java", f.registerBody.path("runtime").asText());
                assertEquals("orbit-java/0.4.0", f.registerBody.path("sdk").asText());
                assertEquals("java-test", f.registerBody.path("worker_id").asText());
                assertEquals(5, f.registerBody.path("contracts").size());
                assertEquals(1, f.unregistrations, "unregister on stop");
            }
        }
    }

    @Test
    @Timeout(30)
    void stopFinishesInFlightTasks() throws Exception {
        try (FakeRuntime f = new FakeRuntime()) {
            synchronized (f) {
                f.tasks.add(FakeRuntime.task("t1", "work", null));
            }
            AtomicBoolean startedWork = new AtomicBoolean();
            Worker w = Worker.builder().server(f.url()).concurrency(1)
                    .pollWait(Duration.ofMillis(100)).logger(QUIET).build();
            w.rawTask("work", "1.0.0", (in, ctx) -> {
                startedWork.set(true);
                Thread.sleep(300);
                return Map.of("done", !ctx.isCancelled());
            });
            w.start();
            waitFor(startedWork::get);
            w.stop();
            synchronized (f) {
                assertEquals("{\"done\":true}", String.valueOf(f.completed.get("t1")));
                assertEquals(1, f.unregistrations);
                assertNull(f.authHeader, "no token configured");
            }
        }
    }

    @Test
    void configurationFromEnvironment() throws Exception {
        try (FakeRuntime f = new FakeRuntime()) {
            Map<String, String> env = Map.of("ORBIT_SERVER", f.url() + "/", "ORBIT_TOKEN", "general",
                    "ORBIT_WORKER_TOKEN", "worker");
            synchronized (f) {
                f.tasks.add(FakeRuntime.task("e1", "echo", Map.of("n", 41)));
            }
            Worker w = Worker.builder().environment(env::get).pollWait(Duration.ofMillis(100)).logger(QUIET).build();
            assertEquals(f.url(), w.server());
            w.task("echo", "1.0.0", EchoIn.class, (in, ctx) -> new EchoOut(in.n() + 1));
            w.start();
            waitFor(() -> {
                synchronized (f) {
                    return f.completed.containsKey("e1");
                }
            });
            w.stop();
            synchronized (f) {
                assertEquals("Bearer worker", f.authHeader, "ORBIT_WORKER_TOKEN wins over ORBIT_TOKEN");
                assertTrue(f.registerBody.path("worker_id").asText().startsWith("java-"));
            }
        }
        Worker defaults = Worker.builder().environment(k -> null).logger(QUIET).build();
        assertEquals(Worker.DEFAULT_SERVER, defaults.server());
    }

    @Test
    @Timeout(60)
    void trustsCaCertificateFromEnvironment(@TempDir Path dir) throws Exception {
        Path keystore = dir.resolve("server.p12");
        Path pem = dir.resolve("ca.pem");
        String keytool = Path.of(System.getProperty("java.home"), "bin", "keytool").toString();
        run(keytool, "-genkeypair", "-alias", "orbit", "-keyalg", "EC", "-groupname", "secp256r1",
                "-dname", "CN=localhost", "-ext", "SAN=dns:localhost,ip:127.0.0.1", "-validity", "2",
                "-keystore", keystore.toString(), "-storetype", "PKCS12", "-storepass", "changeit");
        run(keytool, "-exportcert", "-rfc", "-alias", "orbit", "-file", pem.toString(),
                "-keystore", keystore.toString(), "-storetype", "PKCS12", "-storepass", "changeit");

        KeyStore ks = KeyStore.getInstance("PKCS12");
        try (InputStream in = new FileInputStream(keystore.toFile())) {
            ks.load(in, "changeit".toCharArray());
        }
        KeyManagerFactory kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(ks, "changeit".toCharArray());
        SSLContext serverTls = SSLContext.getInstance("TLS");
        serverTls.init(kmf.getKeyManagers(), null, null);
        HttpsServer https = HttpsServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        https.setHttpsConfigurator(new HttpsConfigurator(serverTls));

        try (FakeRuntime f = new FakeRuntime(https)) {
            synchronized (f) {
                f.tasks.add(FakeRuntime.task("tls", "echo", Map.of("n", 1)));
            }
            Map<String, String> env = Map.of("ORBIT_SERVER", f.url(), "ORBIT_CA_CERT", pem.toString());
            Worker w = Worker.builder().environment(env::get).pollWait(Duration.ofMillis(100)).logger(QUIET).build();
            w.task("echo", "1.0.0", EchoIn.class, (in, ctx) -> new EchoOut(in.n() + 1));
            w.start();
            waitFor(() -> {
                synchronized (f) {
                    return f.completed.containsKey("tls");
                }
            });
            w.stop();
            synchronized (f) {
                assertEquals("{\"n\":2}", f.completed.get("tls").toString());
            }
        }
        assertTrue(Files.size(pem) > 0);
    }

    private static void run(String... cmd) throws Exception {
        Process p = new ProcessBuilder(cmd).redirectErrorStream(true).start();
        String out = new String(p.getInputStream().readAllBytes());
        if (p.waitFor() != 0) {
            fail(String.join(" ", cmd) + " failed: " + out);
        }
    }
}
