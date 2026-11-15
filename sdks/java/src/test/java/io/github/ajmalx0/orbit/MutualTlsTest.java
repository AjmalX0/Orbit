package io.github.ajmalx0.orbit;

import static io.github.ajmalx0.orbit.WorkerTest.QUIET;
import static io.github.ajmalx0.orbit.WorkerTest.waitFor;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;
import static org.junit.jupiter.api.Assertions.fail;

import com.sun.net.httpserver.HttpsConfigurator;
import com.sun.net.httpserver.HttpsParameters;
import com.sun.net.httpserver.HttpsServer;
import java.net.InetSocketAddress;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyStore;
import java.security.cert.CertificateFactory;
import java.time.Duration;
import java.util.Map;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLParameters;
import javax.net.ssl.TrustManagerFactory;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.api.io.TempDir;

/** Mutual TLS against an HTTPS fake runtime that requires a client certificate. */
class MutualTlsTest {
    record EchoIn(int n) {}

    record EchoOut(int n) {}

    @TempDir
    static Path dir;

    /** Generates a CA, an RSA server certificate and an EC client certificate (PKCS#8 keys, openssl 3 defaults). */
    @BeforeAll
    static void certificates() throws Exception {
        openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key", "-out", "ca.pem",
                "-subj", "/CN=orbit-test-ca", "-days", "2",
                "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign");
        issue("server", "/CN=localhost", "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n",
                "-newkey", "rsa:2048");
        issue("client", "/CN=java-mtls", "subjectAltName=DNS:java-mtls\nextendedKeyUsage=clientAuth\n",
                "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1");
        assertTrue(Files.readString(dir.resolve("client.key")).contains("BEGIN PRIVATE KEY"), "PKCS#8 key");
    }

    private static void issue(String name, String subject, String extensions, String... keyArgs) throws Exception {
        Files.writeString(dir.resolve(name + ".ext"), extensions);
        String[] req = {"req", "-nodes", "-keyout", name + ".key", "-out", name + ".csr", "-subj", subject};
        String[] all = new String[req.length + keyArgs.length];
        System.arraycopy(req, 0, all, 0, req.length);
        System.arraycopy(keyArgs, 0, all, req.length, keyArgs.length);
        openssl(all);
        openssl("x509", "-req", "-in", name + ".csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial",
                "-out", name + ".pem", "-days", "2", "-extfile", name + ".ext");
    }

    private static void openssl(String... args) throws Exception {
        String[] cmd = new String[args.length + 1];
        cmd[0] = "openssl";
        System.arraycopy(args, 0, cmd, 1, args.length);
        Process p = new ProcessBuilder(cmd).directory(dir.toFile()).redirectErrorStream(true).start();
        String out = new String(p.getInputStream().readAllBytes());
        if (p.waitFor() != 0) {
            fail(String.join(" ", cmd) + " failed: " + out);
        }
    }

    /** An HTTPS fake runtime that rejects connections without a client certificate signed by the CA. */
    private static FakeRuntime mutualTlsRuntime() throws Exception {
        KeyStore trusted = KeyStore.getInstance("PKCS12");
        trusted.load(null, null);
        trusted.setCertificateEntry("ca", CertificateFactory.getInstance("X.509")
                .generateCertificate(Files.newInputStream(dir.resolve("ca.pem"))));
        TrustManagerFactory tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(trusted);
        SSLContext tls = SSLContext.getInstance("TLS");
        tls.init(Tls.keyManagers(dir.resolve("server.pem"), dir.resolve("server.key")), tmf.getTrustManagers(), null);

        HttpsServer https = HttpsServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        https.setHttpsConfigurator(new HttpsConfigurator(tls) {
            @Override
            public void configure(HttpsParameters params) {
                SSLParameters p = getSSLContext().getDefaultSSLParameters();
                p.setNeedClientAuth(true);
                params.setSSLParameters(p);
            }
        });
        return new FakeRuntime(https);
    }

    @Test
    void loadsPkcs8RsaAndEcKeys() throws Exception {
        assertEquals("RSA", Tls.privateKey(dir.resolve("server.key")).getAlgorithm());
        assertEquals("EC", Tls.privateKey(dir.resolve("client.key")).getAlgorithm());
        Path sec1 = dir.resolve("sec1.key");
        Files.writeString(sec1, "-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----\n");
        Exception e = assertThrows(java.security.GeneralSecurityException.class, () -> Tls.privateKey(sec1));
        assertTrue(e.getMessage().contains("PKCS#8"), e.getMessage());
    }

    @Test
    void clientCertificateNeedsBothFiles() {
        Map<String, String> env = Map.of("ORBIT_CLIENT_CERT", dir.resolve("client.pem").toString());
        assertThrows(IllegalArgumentException.class, () -> Worker.builder().environment(env::get).logger(QUIET).build());
    }

    @Test
    @Timeout(60)
    void workerWithClientCertificateConnects() throws Exception {
        try (FakeRuntime f = mutualTlsRuntime()) {
            synchronized (f) {
                f.tasks.add(FakeRuntime.task("mtls", "echo", Map.of("n", 1)));
            }
            Map<String, String> env = Map.of(
                    "ORBIT_SERVER", f.url(),
                    "ORBIT_CA_CERT", dir.resolve("ca.pem").toString(),
                    "ORBIT_CLIENT_CERT", dir.resolve("client.pem").toString(),
                    "ORBIT_CLIENT_KEY", dir.resolve("client.key").toString());
            Worker w = Worker.builder().environment(env::get).pollWait(Duration.ofMillis(100)).logger(QUIET).build();
            w.task("echo", "1.0.0", EchoIn.class, (in, ctx) -> new EchoOut(in.n() + 1));
            w.start();
            waitFor(() -> {
                synchronized (f) {
                    return f.completed.containsKey("mtls");
                }
            });
            w.stop();
            synchronized (f) {
                assertEquals("{\"n\":2}", f.completed.get("mtls").toString());
                assertEquals("CN=java-mtls", f.peer, "runtime saw the client certificate");
            }
        }
    }

    @Test
    @Timeout(60)
    void builderClientCertificateConnects() throws Exception {
        try (FakeRuntime f = mutualTlsRuntime()) {
            synchronized (f) {
                f.tasks.add(FakeRuntime.task("b", "echo", Map.of("n", 41)));
            }
            Worker w = Worker.builder().environment(k -> null).server(f.url())
                    .caCertFile(dir.resolve("ca.pem"))
                    .clientCert(dir.resolve("client.pem"), dir.resolve("client.key"))
                    .pollWait(Duration.ofMillis(100)).logger(QUIET).build();
            w.task("echo", "1.0.0", EchoIn.class, (in, ctx) -> new EchoOut(in.n() + 1));
            w.start();
            waitFor(() -> {
                synchronized (f) {
                    return f.completed.containsKey("b");
                }
            });
            w.stop();
            synchronized (f) {
                assertEquals("{\"n\":42}", f.completed.get("b").toString());
            }
        }
    }

    @Test
    @Timeout(60)
    void workerWithoutClientCertificateIsRejected() throws Exception {
        try (FakeRuntime f = mutualTlsRuntime()) {
            StringBuffer logged = new StringBuffer();
            System.Logger capture = new System.Logger() {
                @Override
                public String getName() {
                    return "capture";
                }

                @Override
                public boolean isLoggable(Level level) {
                    return true;
                }

                @Override
                public void log(Level level, java.util.ResourceBundle bundle, String msg, Throwable thrown) {
                    logged.append(msg).append('\n');
                }

                @Override
                public void log(Level level, java.util.ResourceBundle bundle, String format, Object... params) {
                    logged.append(format).append('\n');
                }
            };
            Worker w = Worker.builder().environment(k -> null).server(f.url())
                    .caCertFile(dir.resolve("ca.pem"))
                    .pollWait(Duration.ofMillis(100)).logger(capture).build();
            w.task("echo", "1.0.0", EchoIn.class, (in, ctx) -> new EchoOut(in.n()));
            Thread runner = w.start();
            Thread.sleep(2000); // registration keeps failing the TLS handshake and is retried
            assertTrue(runner.isAlive(), "still retrying registration");
            w.stop();
            runner.join(5000);
            assertFalse(runner.isAlive());
            synchronized (f) {
                assertEquals(0, f.registrations, "a worker without a client certificate must not get through");
            }
            String log = logged.toString();
            // With TLS 1.3 the server rejects the missing certificate after the client's side
            // of the handshake, so the client only sees the connection close; the worker says why.
            assertTrue(log.contains("CONNECTION") && log.contains("ORBIT_CLIENT_CERT"), "hint in log: " + log);
        }
    }
}
