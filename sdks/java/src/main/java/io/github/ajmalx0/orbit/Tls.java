package io.github.ajmalx0.orbit;

import java.io.IOException;
import java.io.InputStream;
import java.net.Socket;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.GeneralSecurityException;
import java.security.KeyStore;
import java.security.cert.Certificate;
import java.security.cert.CertificateException;
import java.security.cert.X509Certificate;
import java.util.Arrays;
import java.util.Collection;
import java.util.stream.Stream;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.X509ExtendedTrustManager;

/** Builds a TLS context that trusts a PEM CA in addition to the system trust store. */
final class Tls {
    private Tls() {}

    static SSLContext trusting(Path pem) throws IOException, GeneralSecurityException {
        Collection<? extends Certificate> certs;
        try (InputStream in = Files.newInputStream(pem)) {
            certs = java.security.cert.CertificateFactory.getInstance("X.509").generateCertificates(in);
        }
        if (certs.isEmpty()) {
            throw new CertificateException("no certificate found in " + pem);
        }
        KeyStore store = KeyStore.getInstance(KeyStore.getDefaultType());
        store.load(null, null);
        int i = 0;
        for (Certificate c : certs) {
            store.setCertificateEntry("orbit-ca-" + i++, c);
        }
        X509ExtendedTrustManager custom = trustManager(store);
        X509ExtendedTrustManager system = trustManager(null);
        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(null, new TrustManager[] {new Either(custom, system)}, null);
        return ctx;
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
