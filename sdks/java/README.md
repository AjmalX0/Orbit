# ORBIT Java SDK

Worker SDK for the [ORBIT](../../README.md) runtime. Java 17+, `java.net.http`
and Jackson only.

```xml
<dependency>
  <groupId>io.github.ajmalx0</groupId>
  <artifactId>orbit-sdk</artifactId>
  <version>0.2.0</version>
</dependency>
```

```java
import io.github.ajmalx0.orbit.TaskException;
import io.github.ajmalx0.orbit.Worker;

record Order(String order_id) {}
record Reservation(String reservation_id) {}

Worker worker = Worker.builder().workerId("inventory-java").concurrency(8).build();
worker.task("inventory.reserve", "1.0.0", Order.class, (order, ctx) -> {
    if (order.order_id() == null) {
        throw TaskException.permanent("INVALID_INPUT", "order_id is required");
    }
    // ctx.idempotencyKey() is stable across retries and redeliveries.
    return new Reservation("res-" + order.order_id());
});
// Raw JSON variant: (JsonNode input, TaskContext ctx) -> any JSON object.
worker.rawTask("inventory.release", "1.0.0", (input, ctx) -> Map.of("released", true));

Runtime.getRuntime().addShutdownHook(new Thread(worker::stop));
worker.run(); // returns after stop(): in-flight tasks finish, then the worker unregisters
```

* The attempt is cancelled when the runtime cancels it (timeout or workflow
  cancellation) or when its deadline passes: `ctx.isCancelled()` turns true,
  `ctx.cancellation()` completes, and the handler's thread is interrupted, so
  blocking calls throw `InterruptedException`. `ctx.throwIfCancelled()` is a
  convenient check between units of work.
* Throw `TaskException.permanent(code, msg)`, `TaskException.retryable(code, msg)`
  or `new TaskException(code, msg, retryable, details)` for business errors.
  Other exceptions are reported as retryable `UNHANDLED_EXCEPTION` with the
  stack trace in `details.stack_trace`. A result that is not a JSON object fails
  with `INVALID_RESULT` (not retried); typed input that cannot be decoded fails
  with `INVALID_INPUT` (not retried).
* Results are retried through connection errors and 5xx responses (for up to
  `resultRetryTimeout`, 10 minutes by default), so they survive a runtime
  restart. A lost session (`404 UNKNOWN_SESSION`) is re-registered automatically.
* Configuration comes from `Worker.builder()` or from `ORBIT_SERVER`,
  `ORBIT_WORKER_TOKEN` / `ORBIT_TOKEN`, `ORBIT_CA_CERT` and
  `ORBIT_CLIENT_CERT` / `ORBIT_CLIENT_KEY`. `runtime` defaults to `java`,
  `concurrency` to 4 and `pollWait` to 20s.
* **TLS.** `ORBIT_CA_CERT` (or `.caCertFile(path)`) is a PEM CA trusted in
  addition to the system trust store. For a runtime started with
  `--tls-client-ca`, present a client certificate signed by that CA; the
  runtime identifies the worker by its DNS SAN or subject CN:

  ```java
  Worker.builder()
      .server("https://orbit.internal:7233")
      .caCertFile(Path.of("ca.pem"))
      .clientCert(Path.of("worker.pem"), Path.of("worker.key"))
      .build();
  ```

  or `ORBIT_CLIENT_CERT=worker.pem ORBIT_CLIENT_KEY=worker.key`. The
  certificate file is a PEM chain, leaf first. The key must be unencrypted
  PKCS#8 (`BEGIN PRIVATE KEY`, which openssl 3 writes by default), RSA or EC.
  Convert other formats with `openssl pkcs8 -topk8 -nocrypt -in old.key -out worker.key`.
  `.sslContext(...)` replaces all of this with your own `SSLContext`.
* Logs go to `System.getLogger("orbit")` (java.util.logging unless you bridge
  it); pass `.logger(...)` to use another. Messages logged from a JVM shutdown
  hook may be dropped by java.util.logging's own hook.
* Example worker: [`examples/workers/java`](../../examples/workers/java).
* Tests: `mvn verify`. Release: `mvn -Prelease deploy` (sources, javadoc, GPG
  signatures, Central Portal upload).
