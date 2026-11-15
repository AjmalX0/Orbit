package io.github.ajmalx0.orbit;

import java.io.ByteArrayInputStream;
import java.io.IOException;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.GeneralSecurityException;
import java.security.KeyFactory;
import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.cert.Certificate;
import java.security.cert.CertificateException;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.security.spec.InvalidKeySpecException;
import java.security.spec.PKCS8EncodedKeySpec;
import java.util.Arrays;
import java.util.Base64;
import java.util.Collection;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import java.util.stream.Stream;
import javax.net.ssl.KeyManager;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.X509ExtendedTrustManager;

/**
 * Builds the TLS context: an optional PEM CA trusted in addition to the system trust
 * store, and an optional PEM client certificate and key for mutual TLS.
 */
final class Tls {
    private static final char[] NO_PASSWORD = new char[0];
    private static final Pattern PEM_KEY =
            Pattern.compile("-----BEGIN ([A-Z0-9 ]*PRIVATE KEY)-----([^-]+)-----END \\1-----");

    private Tls() {}

    /**
     * @param caPem   PEM CA certificate(s) to trust, or {@code null} for the system trust store only
     * @param certPem PEM client certificate chain (leaf first), or {@code null} for no client certificate
     * @param keyPem  PKCS#8 PEM private key of the client certificate (RSA or EC)
     */
    static SSLContext context(Path caPem, Path certPem, Path keyPem) throws IOException, GeneralSecurityException {
        TrustManager[] trust = null;
        if (caPem != null) {
            KeyStore store = emptyStore();
            int i = 0;
            for (Certificate c : certificates(caPem)) {
                store.setCertificateEntry("orbit-ca-" + i++, c);
            }
            trust = new TrustManager[] {new Either(trustManager(store), trustManager(null))};
        }
        KeyManager[] keys = certPem != null ? keyManagers(certPem, keyPem) : null;
        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(keys, trust, null);
        return ctx;
    }

    /** Key managers presenting the PEM certificate chain with its PKCS#8 private key. */
    static KeyManager[] keyManagers(Path certPem, Path keyPem) throws IOException, GeneralSecurityException {
        Certificate[] chain = certificates(certPem).toArray(new Certificate[0]);
        PrivateKey key = privateKey(keyPem);
        KeyStore store = emptyStore();
        store.setKeyEntry("orbit-client", key, NO_PASSWORD, chain);
        KeyManagerFactory f = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        f.init(store, NO_PASSWORD);
        return f.getKeyManagers();
    }

    private static Collection<? extends Certificate> certificates(Path pem) throws IOException, CertificateException {
        Collection<? extends Certificate> certs =
                CertificateFactory.getInstance("X.509").generateCertificates(new ByteArrayInputStream(Files.readAllBytes(pem)));
        if (certs.isEmpty()) {
            throw new CertificateException("no certificate found in " + pem);
        }
        return certs;
    }

    static PrivateKey privateKey(Path pem) throws IOException, GeneralSecurityException {
        String text = Files.readString(pem, StandardCharsets.US_ASCII);
        Matcher m = PEM_KEY.matcher(text);
        if (!m.find()) {
            throw new InvalidKeySpecException("no PEM private key found in " + pem);
        }
        if (!m.group(1).equals("PRIVATE KEY")) {
            throw new InvalidKeySpecException(pem + " holds a \"" + m.group(1) + "\"; convert it to PKCS#8 with "
                    + "`openssl pkcs8 -topk8 -nocrypt -in " + pem.getFileName() + "`");
        }
        byte[] der = Base64.getMimeDecoder().decode(m.group(2));
        PKCS8EncodedKeySpec spec = new PKCS8EncodedKeySpec(der);
        for (String algorithm : new String[] {"RSA", "EC"}) {
            try {
                return KeyFactory.getInstance(algorithm).generatePrivate(spec);
            } catch (InvalidKeySpecException e) {
                // try the next algorithm
            }
        }
        throw new InvalidKeySpecException("unsupported private key in " + pem + " (expected PKCS#8 RSA or EC)");
    }

    private static KeyStore emptyStore() throws IOException, GeneralSecurityException {
        KeyStore store = KeyStore.getInstance("PKCS12");
        store.load(null, null);
        return store;
    }

    private static X509ExtendedTrustManager trustManager(KeyStore store) throws GeneralSecurityException {
        TrustManagerFactory f = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        f.init(store);
        for (TrustManager tm : f.getTrustManagers()) {
            if (tm instanceof X509ExtendedTrustManager x) {
                return x;
            }
        }
        throw new GeneralSecurityException("no X509 trust manager available");
    }

    /** Trusts a server chain accepted by either trust manager (hostname checks included). */
    private static final class Either extends X509ExtendedTrustManager {
        private final X509ExtendedTrustManager first;
        private final X509ExtendedTrustManager second;

        Either(X509ExtendedTrustManager first, X509ExtendedTrustManager second) {
            this.first = first;
            this.second = second;
        }

        private interface Check {
            void run(X509ExtendedTrustManager tm) throws CertificateException;
        }

        private void either(Check check) throws CertificateException {
            try {
                check.run(first);
            } catch (CertificateException e) {
                try {
                    check.run(second);
                } catch (CertificateException e2) {
                    e2.addSuppressed(e);
                    throw e2;
                }
            }
        }

        @Override
        public void checkServerTrusted(X509Certificate[] chain, String auth) throws CertificateException {
            either(tm -> tm.checkServerTrusted(chain, auth));
        }

        @Override
        public void checkServerTrusted(X509Certificate[] chain, String auth, Socket socket) throws CertificateException {
            either(tm -> tm.checkServerTrusted(chain, auth, socket));
        }

        @Override
        public void checkServerTrusted(X509Certificate[] chain, String auth, SSLEngine engine) throws CertificateException {
            either(tm -> tm.checkServerTrusted(chain, auth, engine));
        }

        @Override
        public void checkClientTrusted(X509Certificate[] chain, String auth) throws CertificateException {
            either(tm -> tm.checkClientTrusted(chain, auth));
        }

        @Override
        public void checkClientTrusted(X509Certificate[] chain, String auth, Socket socket) throws CertificateException {
            either(tm -> tm.checkClientTrusted(chain, auth, socket));
        }

        @Override
        public void checkClientTrusted(X509Certificate[] chain, String auth, SSLEngine engine) throws CertificateException {
            either(tm -> tm.checkClientTrusted(chain, auth, engine));
        }

        @Override
        public X509Certificate[] getAcceptedIssuers() {
            return Stream.concat(Arrays.stream(first.getAcceptedIssuers()), Arrays.stream(second.getAcceptedIssuers()))
                    .toArray(X509Certificate[]::new);
        }
    }
}
